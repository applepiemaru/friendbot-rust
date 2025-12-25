use futures_util::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use regex::Regex;

use crate::db::Account; // Import Account struct

const BASE_URL: &str = "wss://evertext.sytes.net/socket.io/?EIO=4&transport=websocket";

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum RunMode {
    Daily,
    Handout,
}

#[allow(dead_code)]
pub struct EvertextClient {
    write: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
    read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    ping_interval: u64,
    history: String,
}

#[allow(dead_code)]
#[derive(Debug, PartialEq)]
enum GameState {
    Connected,
    WaitingForCommandPrompt,
    SentD,
    WaitingForRestorePrompt,
    SentCode,
    WaitingForServerList,
    ServerSelected,
    WaitingProcedure,
    RapidFire,
    Finished,
}

impl EvertextClient {
    pub async fn connect(cookie: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut request = BASE_URL.into_client_request()?;
        let headers = request.headers_mut();
        let cookie_header = format!("session={}", cookie);
        headers.insert("Cookie", HeaderValue::from_str(&cookie_header)?);
        headers.insert("User-Agent", HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"));

        println!("[INFO] Connecting to EverText WebSocket...");
        let (mut ws_stream, _) = connect_async(request).await?;

        // 1. Wait for "Open" packet (Type 0) with a timeout
        let msg = tokio::time::timeout(Duration::from_secs(10), ws_stream.next())
            .await
            .map_err(|_| "Connection handshake timed out")?
            .ok_or("Stream closed")??;

        let msg_str = msg.to_string();
        
        if msg_str.starts_with('0') {
            let json_part = &msg_str[1..];
            let data: serde_json::Value = serde_json::from_str(json_part)?;
            
            let sid = data["sid"].as_str().ok_or("No SID found")?.to_string();
            let ping = data["pingInterval"].as_u64().unwrap_or(25000);
            
            println!("[INFO] Connected! Session ID: {}", sid);
            
            // 2. Send "40" to upgrade namespace
            ws_stream.send(Message::Text("40".into())).await?;
            
            let (write, read) = ws_stream.split();

            return Ok(Self {
                write,
                read,
                ping_interval: ping,
                history: String::new(),
            });
        }

        Err("Failed to handshake".into())
    }

    pub async fn run_loop(&mut self, account: &Account, decrypted_code: &str, mode: RunMode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if decrypted_code.is_empty() {
             println!("[ERROR] Code is empty/missing for {}", account.name);
             return Err("MISSING_CODE".into());
        }
        let mut last_ping = Instant::now();
        let mut state = GameState::Connected;
        
        // Trackers
        let mut auto_sent = false;
        let mut handout_sent = false;
        let mut start_sent_at: Option<Instant> = None;

        println!("[INFO][PID:{}] Starting session for account: {} (Mode: {:?})", std::process::id(), account.name, mode);

        let mut heartbeat_check = tokio::time::interval(Duration::from_secs(5));
        let mut last_activity = Instant::now(); // Track game output activity

        loop {
            tokio::select! {
                _ = heartbeat_check.tick() => {
                     // 1. Connection Heartbeat (Ping/Pong)
                     if last_ping.elapsed().as_millis() as u64 > (self.ping_interval + 15000) {
                         println!("[ERROR] Connection timed out (no heartbeat from server). Last ping: {} ms ago", last_ping.elapsed().as_millis());
                         return Err("CONNECTION_TIMEOUT".into());
                     }

                     // 2. Game Activity Timeout
                     if last_activity.elapsed().as_secs() > 180 {
                         println!("[ERROR] Game Activity timed out (stuck for 180s). Disconnecting...");
                         return Err("ACTIVITY_TIMEOUT".into());
                     }

                     // 3. Start Event Retry (Kick if stuck on black screen)
                     if let Some(sent_time) = start_sent_at {
                         if last_activity.elapsed().as_secs() > 20 && sent_time.elapsed().as_secs() > 20 {
                             println!("[WARN] No activity for 20s after 'start'. Retrying initialization...");
                             let start_payload = json!(["start", {"args": ""}]);
                             let _ = self.write.send(Message::Text(format!("42{}", start_payload.to_string()).into())).await;
                             start_sent_at = None; // Only retry once
                         }
                     }
                }
                msg = self.read.next() => {
                    match msg {
                        Some(Ok(m)) => {
                            let text = m.to_string();
                            
                            if text == "2" {
                                self.write.send(Message::Text("3".into())).await?;
                                last_ping = Instant::now();
                            } else if text.starts_with("40") {
                                println!("[INFO] Namespace joined. Initializing session...");
                                
                                println!("[ACTION] Sending 'stop' event...");
                                let stop_payload = json!(["stop", {}]);
                                self.write.send(Message::Text(format!("42{}", stop_payload.to_string()).into())).await?;
                                
                                tokio::time::sleep(Duration::from_millis(1500)).await;

                                println!("[ACTION] Sending 'start' event...");
                                let start_payload = json!(["start", {}]);
                                self.write.send(Message::Text(format!("42{}", start_payload.to_string()).into())).await?;
                                last_activity = Instant::now(); 
                                start_sent_at = Some(Instant::now());
                            } else if text.starts_with("42") {
                                if text.contains("output") {
                                    last_activity = Instant::now();
                                }
                                self.handle_event(&text, &mut state, account, decrypted_code, &mut auto_sent, &mut handout_sent, mode).await?;
                            }
                        }
                        Some(Err(e)) => return Err(e.into()),
                        None => return Err("Socket closed".into()),
                    }
                }
            }
        }
    }

    async fn send_command(&mut self, cmd: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
         let payload = json!(["input", {"input": cmd}]); 
         let packet = format!("42{}", payload.to_string());
         self.write.send(Message::Text(packet.into())).await?;
         Ok(())
    }

    async fn handle_event(&mut self, text: &str, state: &mut GameState, account: &Account, code: &str, auto_sent: &mut bool, handout_sent: &mut bool, mode: RunMode) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let json_part = &text[2..];
        let event: serde_json::Value = match serde_json::from_str(json_part) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        
        if let Some(event_array) = event.as_array() {
            let event_name = event_array.get(0).and_then(|v| v.as_str()).unwrap_or("");
            let event_data = event_array.get(1);

            if event_name == "output" {
                 if let Some(data) = event_data {
                     if let Some(output_text) = data["data"].as_str() {
                         // Print terminal output
                         let clean_log = output_text.replace("\n", " ");
                         if !clean_log.trim().is_empty() {
                             println!("[TERMINAL] {}", clean_log.chars().take(150).collect::<String>());
                         }
                         
                        // Update history for multi-line/chunked parsing
                        self.history.push_str(output_text);
                        if self.history.len() > 10000 {
                            let mut drain_len = self.history.len() - 10000;
                            while !self.history.is_char_boundary(drain_len) && drain_len > 0 { drain_len -= 1; }
                            self.history.replace_range(..drain_len, "");
                        }

                         // --- ROBOT LOGIC USING HISTORY (Handles chunked/split text) ---
                         
                         if self.history.contains("Enter Command to use") {
                             self.history = self.history.replace("Enter Command to use", "[PROCESSED_PROMPT]");
                             match mode {
                                 RunMode::Daily => {
                                     println!("[ACTION] Prompt: 'Enter Command'. Sending 'd'...");
                                     self.send_command("d").await?;
                                 },
                                 RunMode::Handout => {
                                     println!("[ACTION] Prompt: 'Enter Command'. Sending 'ho'...");
                                     self.send_command("ho").await?;
                                 }
                             }
                         }
                         
                         if self.history.contains("Enter Restore code") {
                             self.history = self.history.replace("Enter Restore code", "[PROCESSED_CODE]");
                             println!("[ACTION] Prompt: 'Enter Code'. Sending...");
                             self.send_command(code).await?;
                         }

                         if self.history.contains("Which acc u want to Login") {
                             let target = account.target_server.as_deref().unwrap_or("Default");
                             if target != "Default" {
                                 println!("[ACTION] Server Selection parsing for '{}'...", target);
                                 let re = Regex::new(r"(\d+)-->.*?\((.*?)\)").unwrap();
                                 let mut selected_index = "1".to_string();
                                 let mut found = false;
                                 for cap in re.captures_iter(&self.history) {
                                     if cap[2].contains(target) || (target.to_lowercase() == "all" && cap[2].contains("All of them")) {
                                         selected_index = cap[1].to_string();
                                         found = true; break;
                                     }
                                 }
                                 if !found { println!("[WARN] Target server '{}' not found in list.", target); }
                                 println!("[ACTION] Selecting server index: {}", selected_index);
                                 self.send_command(&selected_index).await?;
                                 self.history = self.history.replace("Which acc u want to Login", "[PROCESSED_SERVER]");
                             }
                         }

                         if self.history.contains("Press y to spend mana on event stages") {
                             self.history = self.history.replace("Press y to spend mana on event stages", "[PROCESSED_MANA]");
                             match mode {
                                 RunMode::Daily => {
                                     println!("[ACTION] Sending 'y' for mana...");
                                     self.send_command("y").await?;
                                 },
                                 RunMode::Handout => {
                                     if !*handout_sent {
                                         self.send_command("ho").await?;
                                         *handout_sent = true;
                                     } else {
                                         self.send_command("y").await?;
                                     }
                                 }
                             }
                         }

                         if self.history.contains("next: Go to the next event") {
                             self.history = self.history.replace("next: Go to the next event", "[PROCESSED_NEXT]");
                             if !*auto_sent {
                                 println!("[ACTION] Sending 'auto'...");
                                 self.send_command("auto").await?;
                                 *auto_sent = true;
                             } else {
                                 println!("[ACTION] Sending 'exit'...");
                                 self.send_command("exit").await?;
                             }
                         }

                         if self.history.contains("DO U WANT TO REFILL MANA") {
                             self.history = self.history.replace("DO U WANT TO REFILL MANA", "[PROCESSED_REFILL]");
                             println!("[ACTION] Sending 'y' for refill...");
                             self.send_command("y").await?;
                         }
                         if self.history.contains("Enter 1, 2 or 3 to select potion") {
                             self.history = self.history.replace("Enter 1, 2 or 3 to select potion", "[PROCESSED_POTION]");
                             self.send_command("3").await?;
                         }
                         if self.history.contains("number of stam100 potions to refill") {
                             self.history = self.history.replace("number of stam100 potions to refill", "[PROCESSED_QTY]");
                             self.send_command("1").await?;
                         }

                         if self.history.contains("Press y to perform more commands") {
                             let low = self.history.to_lowercase();
                             let is_actually_done = low.contains("success") || low.contains("finish") || 
                                                     low.contains("done") || low.contains("already") || 
                                                     *auto_sent || *handout_sent;

                             if is_actually_done {
                                 println!("[INFO] Session complete trigger found in history.");
                                 return Err("SESSION_COMPLETE".into());
                             } else {
                                 println!("[WARN] Exit prompt seen but work not confirmed. Returning to menu...");
                                 self.history = self.history.replace("Press y to perform more commands", "[PROCESSED_Y]");
                                 self.send_command("y").await?;
                             }
                         }

                         // --- ERROR SCANNING (History-based) ---
                         if self.history.contains("Zigza error") || self.history.contains("Incorrect Restore Code") {
                             println!("[ERROR] Zigza/Code Error Detected!");
                             return Err("ZIGZA_DETECTED".into());
                         }
                         if self.history.contains("maximum limit of restore accounts") {
                             println!("[ERROR] Server Full!");
                             return Err("SERVER_FULL".into());
                         }
                         if self.history.contains("restricted only for logged in users") {
                             println!("[ERROR] Cookie Expired!");
                             return Err("LOGIN_REQUIRED".into());
                         }
                         if self.history.contains("Invalid Command") && self.history.contains("Exiting Now") {
                             println!("[ERROR] Invalid Command Loop!");
                             return Err("INVALID_COMMAND_RESTART".into());
                         }
                     }
                 }
            } else if event_name == "idle_timeout" || event_name == "disconnect" {
                return Err(format!("SERVER_{}", event_name.to_uppercase()).into());
            } else if event_name == "activity_ping" || event_name == "user_count_update" {
                return Ok(());
            }
        }
        Ok(())
    }
}
