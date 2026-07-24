use crate::caldav::CalDavClient;
use crate::config::Config;
use crate::models::Output;
use std::io::{self, BufRead, IsTerminal, Write};

/// Store credentials, but only after proving they work — a config file full of
/// a typo'd password is worse than no config file.
pub async fn auth(
    server_url: Option<&str>,
    username: &str,
    app_password: &str,
) -> anyhow::Result<()> {
    let mut config = Config::load()?;
    let server = server_url
        .map(str::to_string)
        .unwrap_or_else(|| config.get_server_url());

    let client = CalDavClient::new(
        server.clone(),
        username.to_string(),
        app_password.to_string(),
    );
    let calendars = client.list_calendars().await?;
    let count = calendars.len();

    config.set_credentials(
        server.clone(),
        username.to_string(),
        app_password.to_string(),
    );
    config.save()?;

    Output::<()>::success_msg(format!(
        "Authenticated as {username} at {server} — found {count} calendar(s)"
    ))
    .print();
    Ok(())
}

/// Read a password from stdin — used when `auth` runs without `--app-password`.
/// Keeping it off the command line avoids exposing it in `ps`, shell history,
/// and the process environment visible to other local users.
pub fn read_password_from_stdin() -> anyhow::Result<String> {
    let stdin = io::stdin();
    if stdin.is_terminal() {
        eprint!("Paste your app-specific password and press Enter: ");
        io::stderr().flush().ok();
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let password = line.trim().to_string();
    if password.is_empty() {
        anyhow::bail!("No password provided on stdin");
    }
    Ok(password)
}
