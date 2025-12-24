Evertale Discord Bot - Setup Guide
==================================

Welcome! This is a Rust-based bot to automate the Evertext terminal game.

Prerequisites
-------------
1. Rust ID (just search "install rust" and download rustup-init.exe).
2. A Discord Bot Token.

Step 1: Configuration
---------------------
1. Rename the file `.env.example` to `.env`.
2. Open `.env` in Notepad and paste your Discord Bot Token:
   DISCORD_TOKEN=your_actual_token_here
   ENCRYPTION_KEY=my_secret_password
   (Make sure to change the encryption key to something secret).

Step 2: Database Setup
----------------------
1. The `db.json` file is empty by default.
2. You will add accounts using Discord commands.
3. ALL RESTORE CODES are encrypted using your `ENCRYPTION_KEY` before being saved to this file.

Step 3: Running the Bot
-----------------------
1. Open a terminal in this folder (Right-click -> Open in Terminal).
2. Run the command:
   cargo run --release

   (The first time will take a few minutes to compile).

Step 4: Bot Commands
--------------------
Once the bot is online in your server:
1. /set_admin_role role:@YourRole
   (Sets you as the admin).

IMPORTANT: Setting the Session Cookie
-------------------------------------
The bot needs a browser session cookie to "see" the game.
1. Log in to the game (evertext.sytes.net) on your browser.
2. Press F12 -> Go to 'Network' tab.
3. Refresh the page. Click the first request (top of the list).
4. Look at 'Request Headers'. Find "Cookie".
5. Copy the value usually starting with "session=...".
   (Copy ONLY the value after "session=", e.g., "ey...").
6. In Discord, run:
   /set_cookies cookie: YOUR_COPIED_SESSION_STRING

Usage
-----
- Add Account: /add_account name:MyAlt code:123456 toggle_server_selection:True server:E-1
- Run Bot: /force_run_all
