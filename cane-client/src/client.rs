use serde::{Deserialize, Serialize, ser::SerializeStruct};
use smart_default::SmartDefault;
use {
    futures_util::{SinkExt, StreamExt},
    std::sync::Arc,
    tokio::sync::{RwLock, broadcast, mpsc},
    tokio_tungstenite::{connect_async, tungstenite::Message},
};

// --- Incoming events ---

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Button {
        key: u8,
        row: u8,
        col: u8,
        edge: ButtonEdge,
    },
    Sonar {
        id: u8,
        dist_cm: f32,
    },
    Climate {
        humidity: f32,
        temp_c: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonEdge {
    Press,
    Release,
}

// --- Persistent state ---

/// Snapshot of the latest known sensor state
#[derive(Debug, Clone, Copy, SmartDefault)]
pub struct State {
    /// Distance read by each sonar in cm
    /// Infinity if out of range
    #[default(_code = "[f32::INFINITY; 3]")]
    pub sonar: [f32; 3],

    /// Humidity percent, temperature in °C
    #[default(_code = "(f32::NAN, f32::NAN)")]
    pub climate: (f32, f32),
    #[default(_code = "[(f32::NAN, f32::NAN); 2]")]
    pub climate_history: [(f32, f32); 2],

    /// True while each button is held
    pub buttons: [bool; 16],
}

impl State {
    /// Update the current state based on an event
    fn update(&mut self, event: Event) {
        match event {
            Event::Button { key, edge, .. } => {
                self.buttons[key as usize] = edge == ButtonEdge::Press
            }
            Event::Sonar { id, dist_cm } => {
                self.sonar[id as usize] = if dist_cm != -1.0 {
                    dist_cm
                } else {
                    f32::INFINITY
                }
            }
            // Event::Climate { humidity, temp_c } => self.climate = Some((humidity, temp_c)),
            Event::Climate { humidity, temp_c } => {
                dbg!(event);
                self.climate = (humidity, temp_c);
                self.climate_history.rotate_right(1);
                self.climate_history[0] = self.climate;
            }
        }
    }
}

#[derive(Clone, Copy, SmartDefault)]
pub struct Config {
    // #[default(true)]
    pub alerts_enabled: bool,
    // #[default(true)]
    pub haptics_enabled: bool,
}

// --- Outgoing commands ---

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Command {
    #[serde(rename = "led")]
    SetLed { key: u8, r: u8, g: u8, b: u8 },
    #[serde(rename = "led")]
    #[serde(serialize_with = "ser_set_all")]
    SetAllLeds { r: u8, g: u8, b: u8 },
    Haptic {
        motor: HapticMotor,
        ms: u32,
        speed: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
pub enum HapticMotor {
    Left,
    Right,
    Both,
}

fn ser_set_all<S: serde::Serializer>(r: &u8, g: &u8, b: &u8, se: S) -> Result<S::Ok, S::Error> {
    let mut se = se.serialize_struct("led", 4)?;
    se.serialize_field("r", r)?;
    se.serialize_field("g", g)?;
    se.serialize_field("b", b)?;
    se.serialize_field("all", &true)?;
    se.end()
}

// --- CaneClient ---

#[derive(Clone)]
pub struct CaneClient {
    pub config: Arc<RwLock<Config>>,
    state: Arc<RwLock<State>>,
    events_tx: broadcast::Sender<Event>,
    cmd_tx: mpsc::Sender<Command>,
}

impl CaneClient {
    #![allow(dead_code)]

    pub async fn connect(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (ws_stream, _) = connect_async(url).await?;
        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        let state = Arc::new(RwLock::new(State::default()));
        let (events_tx, _) = broadcast::channel::<Event>(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<Command>(32);

        let send_state = Arc::clone(&state);
        let send_events_tx = events_tx.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    // Handle incoming message
                    msg = ws_rx.next() => {
                        match msg {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(event) = serde_json::from_str(&text) {
                                    send_state.write().await.update(event);
                                    let _ = send_events_tx.send(event);
                                }
                            }
                            Some(Ok(_)) => {} // Other message; ignore
                            _ => break, // None or Err; connection closed
                        }
                    }
                    cmd = cmd_rx.recv() => {
                        let Some(cmd) = cmd else { break };
                        let payload = serde_json::to_string(&cmd).unwrap();
                        let _ = ws_tx.send(Message::Text(payload.into())).await;
                    }
                }
            }
        });

        Ok(Self {
            state,
            config: Arc::new(RwLock::new(Config::default())),
            events_tx,
            cmd_tx,
        })
    }

    /// Await a snapshot of the current state
    pub async fn state(&self) -> State {
        *self.state.read().await
    }

    /// Await a snapshot of the current config
    pub async fn config(&self) -> Config {
        *self.config.read().await
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    pub async fn send_cmd(&self, cmd: Command) -> Result<(), mpsc::error::SendError<Command>> {
        self.cmd_tx.send(cmd).await
    }

    pub async fn set_led(
        &self,
        key: u8,
        r: u8,
        g: u8,
        b: u8,
    ) -> Result<(), mpsc::error::SendError<Command>> {
        self.send_cmd(Command::SetLed { key, r, g, b }).await
    }

    pub async fn set_all_leds(
        &self,
        r: u8,
        g: u8,
        b: u8,
    ) -> Result<(), mpsc::error::SendError<Command>> {
        self.send_cmd(Command::SetAllLeds { r, g, b }).await
    }

    pub async fn set_haptic(
        &self,
        motor: HapticMotor,
        ms: u32,
        speed: u8,
    ) -> Result<(), mpsc::error::SendError<Command>> {
        self.send_cmd(Command::Haptic { motor, ms, speed }).await
    }
}
