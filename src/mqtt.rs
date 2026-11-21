use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, Transport};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::config::Connection;
use crate::error::{Error, Result, redact};
use crate::status::{self, StatusReport};

const PACKET_BYTES: usize = 512 * 1024;

enum IncomingMsg {
    Live,
    Publish(String),
    Dead(String),
}

pub struct Session {
    client: AsyncClient,
    incoming: mpsc::Receiver<IncomingMsg>,
    task: JoinHandle<()>,
    serial: String,
    seq: u64,
    state: Map<String, Value>,
    secret: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Session {
    pub async fn connect(conn: &Connection) -> Result<Self> {
        preflight(conn).await?;
        let client_id = format!(
            "h2d-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
                % 100_000
        );
        // The printer presents an X.509 v1 certificate whose CN is the serial, not the IP.
        // OpenSSL accepts that version. rustls rejects it before a custom verifier runs.
        // Hostname checks fail because we dial an IP. The access code is the LAN credential.
        // This is the Developer Mode path, on the local network.
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|err| Error::msg(format!("TLS setup failed: {err}")))?;

        let mut options = MqttOptions::new(client_id, conn.ip.clone(), conn.mqtt_port);
        options.set_credentials("bblp", conn.access_code.clone());
        options.set_keep_alive(Duration::from_secs(30));
        options.set_clean_session(true);
        options.set_max_packet_size(PACKET_BYTES, PACKET_BYTES);
        options.set_transport(Transport::tls_with_config(tls.into()));

        let (client, mut eventloop) = AsyncClient::new(options, 16);
        let (tx, rx) = mpsc::channel(256);
        let secret = conn.access_code.clone();
        let task = tokio::spawn(async move {
            let mut announced = false;
            loop {
                match eventloop.poll().await {
                    Ok(event) => {
                        if !announced {
                            announced = true;
                            if tx.send(IncomingMsg::Live).await.is_err() {
                                break;
                            }
                        }
                        if let Event::Incoming(Incoming::Publish(publish)) = event {
                            let text = String::from_utf8_lossy(&publish.payload);
                            let text = text.trim_matches(char::from(0)).trim().to_string();
                            if text.is_empty() {
                                continue;
                            }
                            if tx.send(IncomingMsg::Publish(text)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = tx.send(IncomingMsg::Dead(err.to_string())).await;
                        break;
                    }
                }
            }
        });

        let mut session = Session {
            client,
            incoming: rx,
            task,
            serial: conn.serial.clone(),
            seq: 0,
            state: Map::new(),
            secret,
        };
        session.wait_live(conn.timeout).await?;
        let report = format!("device/{}/report", conn.serial);
        session
            .client
            .subscribe(report, QoS::AtLeastOnce)
            .await
            .map_err(|err| session.fail(err.to_string()))?;
        Ok(session)
    }

    pub async fn status(&mut self, timeout_for: Duration) -> Result<StatusReport> {
        let seq = self.next_seq();
        let payload = json!({
            "pushing": {
                "sequence_id": seq,
                "command": "pushall",
                "version": 1,
                "push_target": 1
            }
        });
        self.publish(&payload).await?;
        let deadline = Instant::now() + timeout_for;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match timeout(remaining, self.incoming.recv()).await {
                Ok(Some(IncomingMsg::Publish(text))) => {
                    self.absorb(&text);
                    if self.state.contains_key("gcode_state") {
                        return Ok(status::report_from_state(&self.state));
                    }
                }
                Ok(Some(IncomingMsg::Dead(err))) => return Err(self.fail(err)),
                Ok(Some(IncomingMsg::Live) | None) => {
                    return Err(self.fail("MQTT connection closed while reading status"));
                }
                Err(_) => break,
            }
        }
        if self.state.contains_key("gcode_state") {
            return Ok(status::report_from_state(&self.state));
        }
        Err(self.fail(
            "timed out waiting for a status report. The printer accepted MQTT but sent no push_status. Confirm LAN Only and Developer Mode are both on.",
        ))
    }

    pub async fn print_command(&mut self, command: &str, timeout_for: Duration) -> Result<Value> {
        let seq = self.next_seq();
        let payload = json!({
            "print": {
                "sequence_id": seq,
                "command": command,
                "param": ""
            }
        });
        self.publish(&payload).await?;
        self.wait_ack("print", &seq, timeout_for).await
    }

    pub async fn publish_print(&mut self, body: Value, timeout_for: Duration) -> Result<Value> {
        let seq = body
            .pointer("/print/sequence_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::msg("print payload is missing sequence_id"))?
            .to_string();
        self.publish(&body).await?;
        self.wait_ack("print", &seq, timeout_for).await
    }

    pub fn next_seq(&mut self) -> String {
        self.seq += 1;
        self.seq.to_string()
    }

    pub fn report(&self) -> StatusReport {
        status::report_from_state(&self.state)
    }

    pub async fn wait_until<F>(
        &mut self,
        timeout_for: Duration,
        mut pred: F,
    ) -> Result<StatusReport>
    where
        F: FnMut(&StatusReport) -> bool,
    {
        let deadline = Instant::now() + timeout_for;
        loop {
            let current = self.report();
            if pred(&current) && !current.gcode_state.is_empty() {
                return Ok(current);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(self.report());
            }
            let slice = remaining.min(Duration::from_secs(2));
            match timeout(slice, self.incoming.recv()).await {
                Ok(Some(IncomingMsg::Publish(text))) => {
                    self.absorb(&text);
                }
                Ok(Some(IncomingMsg::Dead(err))) => return Err(self.fail(err)),
                Ok(Some(IncomingMsg::Live) | None) => {
                    return Err(self.fail("MQTT connection closed"));
                }
                Err(_) => {}
            }
        }
    }

    async fn wait_live(&mut self, timeout_for: Duration) -> Result<()> {
        match timeout(timeout_for, self.incoming.recv()).await {
            Ok(Some(IncomingMsg::Live)) => Ok(()),
            Ok(Some(IncomingMsg::Publish(text))) => {
                self.absorb(&text);
                Ok(())
            }
            Ok(Some(IncomingMsg::Dead(err))) => Err(self.explain_dead(err)),
            Ok(None) => Err(self.fail("MQTT task stopped before connect")),
            Err(_) => Err(self.fail(format!(
                "timed out opening MQTT to the printer after {}s",
                timeout_for.as_secs()
            ))),
        }
    }

    async fn wait_ack(
        &mut self,
        category: &str,
        sequence_id: &str,
        timeout_for: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout_for;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(self.fail(format!(
                    "timed out waiting for an acknowledgement of {category} sequence {sequence_id}"
                )));
            }
            match timeout(remaining, self.incoming.recv()).await {
                Ok(Some(IncomingMsg::Publish(text))) => {
                    let value = self.absorb(&text);
                    if status::is_ack(&value, category, sequence_id) {
                        return Ok(value);
                    }
                }
                Ok(Some(IncomingMsg::Dead(err))) => return Err(self.fail(err)),
                Ok(Some(IncomingMsg::Live) | None) => {
                    return Err(
                        self.fail("MQTT connection closed while waiting for an acknowledgement")
                    );
                }
                Err(_) => {
                    return Err(self.fail(format!(
                        "timed out waiting for an acknowledgement of {category} sequence {sequence_id}"
                    )));
                }
            }
        }
    }

    async fn publish(&self, payload: &Value) -> Result<()> {
        let topic = format!("device/{}/request", self.serial);
        let bytes = serde_json::to_vec(payload)?;
        self.client
            .publish(topic, QoS::AtLeastOnce, false, bytes)
            .await
            .map_err(|err| self.fail(err.to_string()))
    }

    fn absorb(&mut self, text: &str) -> Value {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Value::Null;
        };
        status::merge_report(&mut self.state, &value);
        value
    }

    fn explain_dead(&self, err: String) -> Error {
        let err = redact(&err, &self.secret);
        let lower = err.to_ascii_lowercase();
        if lower.contains("notauthorized")
            || lower.contains("not authorized")
            || lower.contains("bad user")
        {
            return self.fail(
                "MQTT login failed. Check the access code. LAN Only must be on, and the code is the one shown on the printer.",
            );
        }
        if lower.contains("connection refused") {
            return self.fail(
                "printer refused MQTT on port 8883. Turn on LAN Only. With it off, the broker does not accept a connection.",
            );
        }
        self.fail(format!("MQTT connection failed: {err}"))
    }

    fn fail(&self, text: impl Into<String>) -> Error {
        Error::msg(redact(&text.into(), &self.secret))
    }
}

async fn preflight(conn: &Connection) -> Result<()> {
    let addr = format!("{}:{}", conn.ip, conn.mqtt_port);
    let timeout_for = conn.timeout.min(Duration::from_secs(5));
    match timeout(timeout_for, tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(stream)) => {
            drop(stream);
            Ok(())
        }
        Ok(Err(err)) => Err(Error::msg(format!(
            "cannot reach {addr}: {err}. The printer must be on this LAN."
        ))),
        Err(_) => Err(Error::msg(format!("timed out reaching {addr}"))),
    }
}
