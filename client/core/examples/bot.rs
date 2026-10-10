//! Тестовый бот на feather-core: подключается к шлюзу и действует от имени пользователя.
//!
//!   cargo run -p feather-core --example bot -- <сервер> <токен> typing <канал> <секунд>
//!   cargo run -p feather-core --example bot -- <сервер> <токен> say <канал> <текст>

use std::time::Duration;

use feather_core::{Api, GatewayEvent};
use feather_protocol::{ClientOp, Snowflake};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [server, token, action, channel, rest @ ..] = args.as_slice() else {
        eprintln!("использование: bot <сервер> <токен> typing|say <канал> <секунд|текст>");
        std::process::exit(2);
    };
    let api = Api::new(server).with_token(token.clone());
    let channel: Snowflake = channel.parse().expect("канал: число");
    match action.as_str() {
        "say" => {
            let m = api.send_message(channel, &rest.join(" "), "bot".into()).await.expect("отправка");
            println!("отправлено {}", m.id);
        }
        "typing" => {
            let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(10);
            let (tx, mut ready) = tokio::sync::mpsc::unbounded_channel();
            let gw = feather_core::gateway::spawn(api.gateway_url(), token.clone(), move |ev| {
                if let GatewayEvent::Ready { .. } = ev {
                    let _ = tx.send(());
                }
            });
            ready.recv().await;
            // Как настоящий клиент: сигнал раз в 8 с, пока «печатаем»
            let until = tokio::time::Instant::now() + Duration::from_secs(secs);
            while tokio::time::Instant::now() < until {
                gw.send(ClientOp::Typing { channel_id: channel });
                println!("печатаю…");
                tokio::time::sleep(Duration::from_secs(8)).await;
            }
        }
        other => eprintln!("неизвестное действие {other}"),
    }
}
