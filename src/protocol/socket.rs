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
        let mut last_ping = Instant::now();
        let mut state = GameState::Connected;
        
        // Trackers
        let mut auto_sent = false;
        let mut handout_sent = false;

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

                     // 2. Game Activity Timeout (Stuck on 'start' or unresponsive script)
                     // If we haven't received any 'output' from the game in 120 seconds, assume stuck.
                     if last_activity.elapsed().as_secs() > 120 {
                         println!("[ERROR] Game Activity timed out (stuck for 120s). Disconnecting...");
                         return Err("ACTIVITY_TIMEOUT".into());
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
                                // ... (existing code)
                                println!("[INFO] Namespace joined. Initializing session...");
                                let stop_payload = json!(["stop", {}]);
                                self.write.send(Message::Text(format!("42{}", stop_payload.to_string()).into())).await?;
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                println!("[ACTION] Sending 'start' event...");
                                let start_payload = json!(["start", {"args": ""}]);
                                self.write.send(Message::Text(format!("42{}", start_payload.to_string()).into())).await?;
                                last_activity = Instant::now(); // Reset activity on start
                            } else if text.starts_with("42") {
                                // If we get actual game data, update activity
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
        // Parse the event. If it fails, just ignore it (sometimes random packets come in)
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
                         // Print terminal output (clean up newlines for log readability)
                         let clean_log = output_text.replace("\n", " ");
                         // Log only significant chunks to avoid spam
                         if clean_log.len() > 5 {
                             println!("[TERMINAL] {}", clean_log.chars().take(150).collect::<String>());
                         }
                         
                        // Update history for multi-line parsing
                        self.history.push_str(output_text);
                        if self.history.len() > 10000 {
                            let mut drain_len = self.history.len() - 10000;
                            while !self.history.is_char_boundary(drain_len) && drain_len > 0 {
                                drain_len -= 1;
                            }
                            self.history.replace_range(..drain_len, "");
                        }

                         // --- 1. Initial / Login Flow ---
                         if output_text.contains("Enter Command to use") {
                             match mode {
                                 RunMode::Daily => {
                                     println!("[ACTION] Prompt: 'Enter Command'. Sending 'd' (Daily)...");
                                     *state = GameState::SentD;
                                     self.send_command("d").await?;
                                 },
                                 RunMode::Handout => {
                                     println!("[ACTION] Prompt: 'Enter Command'. Sending 'ho' (Handout)...");
                                     // State SentD is roughly equivalent to SentHo for flow purposes
                                     *state = GameState::SentD; 
                                     self.send_command("ho").await?;
                                 }
                             }
                         }
                         
                         if output_text.contains("Enter Restore code") {
                             println!("[ACTION] Prompt: 'Enter Restore code'. Sending Code...");
                             *state = GameState::SentCode;
                             self.send_command(code).await?;
                         }

                         // Server Selection
                         if output_text.contains("Which acc u want to Login") {
                             if let Some(target) = &account.target_server {
                                 println!("[ACTION] Prompt: 'Server Selection'. Parsing for '{}'...", target);
                                 let mut selected_index = "1".to_string();
                                 let re = Regex::new(r"(\d+)-->.*?\((.*?)\)").unwrap();
                                 let mut found = false;
                                 
                                 for cap in re.captures_iter(&self.history) {
                                     let index = &cap[1];
                                     let server_name = &cap[2];
                                     if server_name.contains(target) || (target.to_lowercase() == "all" && server_name.contains("All of them")) {
                                         println!("[INFO] Found target server '{}' at index {}", target, index);
                                         selected_index = index.to_string();
                                         found = true;
                                         break;
                                     }
                                 }
                                 if !found { println!("[WARN] Target '{}' not found. Defaulting to '1'.", target); }
                                 
                                 println!("[ACTION] Sending server choice: {}", selected_index);
                                 self.send_command(&selected_index).await?;
                                 *state = GameState::ServerSelected;
                             } else {
                                 println!("[INFO] No targetServer specified. Assuming single server - waiting for terminal to auto-select.");
                                 // Do NOT send any command. Terminal handles it.
                             }
                         }

                         // --- 2. Main Game Flow ---
                         
                         // "Press y to spend mana on event stages :"
                         if output_text.contains("Press y to spend mana on event stages") {
                             match mode {
                                 RunMode::Daily => {
                                     println!("[ACTION] Prompt: 'Spend mana'. Sending 'y'...");
                                     self.send_command("y").await?;
                                 },
                                 RunMode::Handout => {
                                     if !*handout_sent {
                                         println!("[ACTION] Prompt: 'Spend mana'. Sending 'ho' (Handout)...");
                                         self.send_command("ho").await?;
                                         *handout_sent = true;
                                     } else {
                                         println!("[ACTION] Prompt: 'Spend mana'. Sending 'y' (Handout Confirmation)...");
                                         self.send_command("y").await?;
                                     }
                                 }
                             }
                         }

                         // "next: Go to the next event. [default option if nothing entered]"
                         if output_text.contains("next: Go to the next event") {
                             if !*auto_sent {
                                 println!("[ACTION] Prompt: 'next event'. Sending 'auto' (First time)...");
                                 self.send_command("auto").await?;
                                 *auto_sent = true;
                             } else {
                                 println!("[ACTION] Prompt: 'next event'. Sending 'exit' (Already sent auto)...");
                                 self.send_command("exit").await?;
                             }
                         }

                         // --- 3. Mana Refill Logic (Situational) ---
                         // "DO U WANT TO REFILL MANA ? (press y to refill):"
                         // "DO U WANT TO REFILL MANA ? (press y to refill):"
                         if output_text.contains("DO U WANT TO REFILL MANA") {
                             println!("[ACTION] Prompt: 'Refill Mana'. Sending 'y'...");
                             self.send_command("y").await?;
                         }

                         // "Enter 1, 2 or 3 to select potion to refill:"
                         if output_text.contains("Enter 1, 2 or 3 to select potion to refill") {
                             println!("[ACTION] Prompt: 'Select potion'. Sending '3'...");
                             self.send_command("3").await?;
                         }

                         // "Enter the number of stam100 potions to refill"
                         if output_text.contains("number of stam100 potions to refill") {
                             println!("[ACTION] Prompt: 'Potion quantity'. Sending '1'...");
                             self.send_command("1").await?;
                         }

                         // --- 4. More Events Prompt ---
                         // "Press y to do more events:"
                         // User logic: "we will write 'y' and now the terminal will ask for 'next: ...' now we will write 'exit'"
                         if output_text.contains("Press y to do more events") {
                             println!("[ACTION] Prompt: 'Do more events?'. Sending 'y' (waiting for 'next' prompt to exit)...");
                             self.send_command("y").await?;
                             // We do NOT send 'exit' here. We wait for the "next: Go to the next event" prompt to appear again.
                             // Since 'auto_sent' is already true, the 'next' block above will handle sending 'exit'.
                         }

                         // --- 5. End of Loop ---
                         // "Press y to perform more commands:"
                         if output_text.contains("Press y to perform more commands") {
                             println!("[INFO] Prompt: 'Perform more commands'. Run Complete.");
                             return Err("SESSION_COMPLETE".into()); // Trigger clean exit
                         }

                         // --- 6. Error Handling ---
                         
                         // "Invalid Command ... Exiting Now"
                         if output_text.contains("Invalid Command") && output_text.contains("Exiting Now") {
                             println!("[ERROR] Invalid Command Detected. Triggering Restart...");
                             return Err("INVALID_COMMAND_RESTART".into());
                         }

                         if output_text.contains("Either Zigza error or Incorrect Restore Code Entered") {
                             println!("[ERROR] Zigza Error Detected!");
                             return Err("ZIGZA_DETECTED".into());
                         }

                         if output_text.contains("Server reached maximum limit of restore accounts") {
                             println!("[ERROR] Server Full Detected!");
                             return Err("SERVER_FULL".into());
                         }

                         if output_text.contains("Access to start bot is restricted only for logged in users") {
                             println!("[ERROR] Login Required / Cookie Expired!");
                             return Err("LOGIN_REQUIRED".into());
                         }
                     }
                 }
            } else if event_name == "idle_timeout" {
                println!("[ERROR] Server sent 'idle_timeout'. Disconnecting...");
                return Err("IDLE_TIMEOUT".into());
            } else if event_name == "connection_failed" {
                println!("[ERROR] Server sent 'connection_failed'. Disconnecting...");
                return Err("CONNECTION_FAILED".into());
            } else if event_name == "disconnect" {
                println!("[ERROR] Server sent 'disconnect' event.");
                return Err("SERVER_DISCONNECT".into());
            } else if event_name == "activity_ping" {
                // Ignore heartbeat/ping events
                return Ok(());
            } else {
                println!("[DEBUG] Unhandled Socket.io event: {} -> {:?}", event_name, event_data);
            }
        }
        Ok(())
    }
}
