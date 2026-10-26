//! Silent operator check of the production Rust connection. Never dials or
//! supplies owner context. OPENAI_API_KEY must come from the environment.
use augmentagent_phone::{live_connection::LiveConnection, live_session::{CloseReason, LiveInput}};
#[tokio::main]
async fn main() {
    let Ok(key) = std::env::var("OPENAI_API_KEY") else {
        eprintln!("OPENAI_API_KEY is required");
        std::process::exit(1);
    };
    let result = async {
        let connection = LiveConnection::connect(&key, "Readiness check. Remain silent and do not delegate.").await?;
        connection.finish().await
    }.await;
    match result {
        Ok(LiveInput::Closed { reason: CloseReason::Requested, seconds: Some(seconds) }) => {
            println!("gpt-live-1 readiness confirmed; final_seconds={seconds}");
        }
        Err(error) => { eprintln!("readiness failed: {}", error.code()); std::process::exit(1); }
        _ => { eprintln!("readiness failed: missing final usage"); std::process::exit(1); }
    }
}
