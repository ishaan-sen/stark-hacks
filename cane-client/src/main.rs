mod client;

use openai_tools::{
    chat::request::ChatCompletion,
    common::{
        message::{Content, Message},
        models::ChatModel,
        role::Role,
    },
};
use std::{path::PathBuf, sync::LazyLock};
use tokio::{fs, process, sync::Mutex};

use client::{ButtonEdge, CaneClient, Event, HapticMotor};

type Error = Box<dyn std::error::Error>;

mod kp {
    //! Keypad button index constants
    //! A-D: Button panel columns
    //! 0-3: Button panel rows

    #![allow(dead_code)]

    pub const A0: u8 = 0;
    pub const B0: u8 = 1;
    pub const C0: u8 = 2;
    pub const D0: u8 = 3;
    pub const A1: u8 = 4;
    pub const B1: u8 = 5;
    pub const C1: u8 = 6;
    pub const D1: u8 = 7;
    pub const A2: u8 = 8;
    pub const B2: u8 = 9;
    pub const C2: u8 = 10;
    pub const D2: u8 = 11;
    pub const A3: u8 = 12;
    pub const B3: u8 = 13;
    pub const C3: u8 = 14;
    pub const D3: u8 = 15;

    pub const DESCRIBE: u8 = D3;
    pub const TOGGLE_ALERTS: u8 = D2;
    pub const TOGGLE_HAPTICS: u8 = D1;
}

mod prompts {
    pub const DESCRIBE: &str = r#"I am blind. This camera feed shows my point of view. Briefly describe what is immediately in front of me, in the center of the view. Focus on what is most important and pertinent. No need for complete sentences, keep it sharp and to-the-point."#;
    pub const ALERT: &str = r#"I am blind. This camera feed shows my point of view. If there is any immediate danger or obstacle, or something I should keep in mind or know about for safety purposes, please let me know. In addition to physical barriers, this could include, for instance, wet floor signs or road crossings. Remember to be brief and direct; no need for full sentences, just get the point across as efficiently as possible. If you provide such an alert, make sure to begin your message with "[alert]". If there is nothing worth noting, respond with "[none]" and nothing else."#;
}

const TMP_DIR: &str = "tmp";
const CAPTURE_NAME: &str = "cane-capture.jpeg";

#[cfg(target_os = "macos")]
const CAPTURE_CMD: &str = "imagesnap";
#[cfg(target_os = "macos")]
const SPEAK_CMD: &str = "say";

#[cfg(any(target_os = "android", target_os = "linux"))]
const CAPTURE_CMD: &str = "termux-camera-photo";
#[cfg(any(target_os = "android", target_os = "linux"))]
const SPEAK_CMD: &str = "termux-tts-speak";

static VISION: LazyLock<Mutex<ChatCompletion>> = LazyLock::new(|| {
    use std::env::var;
    let key = var("API_KEY").unwrap();
    let endpoint = var("API_ENDPOINT").unwrap();
    let model = var("MODEL").unwrap();
    let mut chat = ChatCompletion::with_url(&endpoint, &key);
    chat.model(ChatModel::custom(model));
    Mutex::new(chat)
});

async fn sleep_millis(millis: u64) {
    tokio::time::sleep(tokio::time::Duration::from_millis(millis)).await
}

#[tokio::main]
pub async fn main() -> Result<(), Error> {
    dotenvy::dotenv().unwrap();
    fs::create_dir_all(TMP_DIR).await?;

    let url_arg = std::env::args().nth(1);
    let url = url_arg.as_deref().unwrap_or("ws://cane.local:81");

    eprintln!("Connecting to {url}...");
    let cane = CaneClient::connect(url).await?;
    eprintln!("Connected.");

    speak("ba bing");

    // Flash buttons green on connect
    cane.set_all_leds(0, 20, 0).await?;
    sleep_millis(500).await;
    cane.set_all_leds(0, 0, 0).await?;

    let alert_loop_cane = cane.clone();
    tokio::spawn(async move {
        loop {
            if alert_loop_cane.config.read().await.alerts_enabled {
                // query_vision(VisionQuery::Alert).await;
                tokio::spawn(query_vision(VisionQuery::Alert));
                sleep_millis(30_000).await;
            } else {
                sleep_millis(1_000).await;
            }
        }
    });

    let mut events_rx = cane.subscribe();
    loop {
        handle_event(events_rx.recv().await?, &cane).await?;
    }

    // No need to return `Ok(())` since an indefinite `loop` has never type
}

/// Spawns a new async task to speak some text
/// Will not affect the rest of the program upon failure
fn speak(text: impl ToString) {
    let text = text.to_string();
    tokio::spawn(async move {
        process::Command::new(SPEAK_CMD)
            .arg(text)
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
    });
}

async fn handle_event(event: Event, cane: &CaneClient) -> Result<(), Error> {
    match event {
        Event::Button {
            key,
            row,
            col,
            edge,
        } => {
            println!("{event:?}");
            tokio::spawn(handle_button(key, row, col, edge, cane.clone()));
        }
        Event::Sonar { .. } => proximity_haptics(cane).await?,
        Event::Climate { .. } => handle_climate(cane).await?,
    }

    Ok(())
}

async fn handle_climate(cane: &CaneClient) -> Result<(), Error> {
    let [(cur_hum, cur_temp), (last_hum, last_temp), ..] = cane.state().await.climate_history;

    if cur_hum >= last_hum + 2.0 && (cur_temp - last_temp).abs() <= 1.0 {
        speak("There might be a puddle over there.");
    }

    Ok(())
}

async fn proximity_haptics(cane: &CaneClient) -> Result<(), Error> {
    if !cane.config.read().await.haptics_enabled {
        return Ok(());
    }

    fn rumble_curve(dist: f32) -> f32 {
        (-(dist - 50.0) / 55.0).exp() * (dist <= 200.0) as u32 as f32
    }

    let [center, right, left] = cane.state().await.sonar;

    for (dist, motor) in [
        (left.min(center), HapticMotor::Left),
        (right.min(center), HapticMotor::Right),
    ] {
        const DEADZONE: f32 = 0.1;

        let rumble = rumble_curve(dist);
        if rumble > DEADZONE {
            let rumble_byte = (rumble * 255.0).round().clamp(0.0, 255.0) as u8;
            cane.set_haptic(motor, 100, rumble_byte).await?;
        }
    }

    Ok(())
}

async fn handle_button(key: u8, _row: u8, _col: u8, edge: ButtonEdge, cane: CaneClient) {
    if edge != ButtonEdge::Press {
        return;
    }

    if key == kp::DESCRIBE {
        query_vision(VisionQuery::Describe).await;
    }

    if key == kp::TOGGLE_ALERTS {
        toggle_config(&cane, |cfg| &mut cfg.alerts_enabled, "Alerts").await;
    }

    if key == kp::TOGGLE_HAPTICS {
        toggle_config(&cane, |cfg| &mut cfg.haptics_enabled, "Haptics").await;
    }
}

async fn toggle_config(
    cane: &CaneClient,
    field: impl FnOnce(&mut client::Config) -> &mut bool,
    field_name: impl ToString,
) {
    let mut config = cane.config.write().await;
    let field = field(&mut config);
    if *field {
        *field = false;
        speak(format!("{} disabled.", field_name.to_string()));
    } else {
        *field = true;
        speak(format!("{} enabled.", field_name.to_string()));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisionQuery {
    /// Describe the object ahead
    Describe,
    /// Inform the user of any potential safety concerns, if applicable
    Alert,
}

async fn query_vision(query: VisionQuery) {
    // --- Step 1: Capture image to a file ---
    let capture_path = PathBuf::from(TMP_DIR)
        .join(CAPTURE_NAME)
        // This step is necessary because openai_tools requires something implementing `AsRef<str>` instead of `AsRef<OsStr>`. Maybe should be PR'd
        .to_string_lossy()
        .to_string();
    let mut capture_cmd = process::Command::new(CAPTURE_CMD);
    capture_cmd.arg(&capture_path);
    capture_cmd
        .spawn()
        .expect("Failed to create image capture command")
        .wait()
        .await
        .expect("Failed to capture camera feed");

    // --- Step 2: Ask vision model to describe ---

    let prompt = match query {
        VisionQuery::Describe => prompts::DESCRIBE,
        VisionQuery::Alert => prompts::ALERT,
    };

    let messages = [Message::from_message_array(
        Role::User,
        [
            Content::from_text(prompt),
            Content::from_image_file(&capture_path),
        ]
        .into(),
    )];

    let response = VISION
        .lock()
        .await
        .messages(messages.into())
        .chat()
        .await
        .expect("Failed to query model");

    // --- Step 3: Speak the response ---
    let output: &str = response.choices[0]
        .message
        .content
        .as_ref()
        .expect("Model response had no content")
        .text
        .as_deref()
        .expect("Model response had no text content");

    dbg!(output);

    if output == "[none]" {
        return;
    }

    let to_say = match query {
        VisionQuery::Describe => output.trim(),
        VisionQuery::Alert if let Some((_, s)) = output.split_once("[alert]") => s.trim(),
        _ => return,
    };
    speak(to_say);
}
