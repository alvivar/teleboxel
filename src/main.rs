use axum::{Router, response::IntoResponse, routing::get};
use fastwebsockets::{Frame, OpCode, WebSocketError, upgrade};

#[tokio::main]
async fn main() {
    let app = Router::new().route("/", get(ws_handler));
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: upgrade::IncomingUpgrade) -> impl IntoResponse {
    let (response, fut) = ws.upgrade().unwrap();
    tokio::task::spawn(async move {
        if let Err(e) = handle_client(fut).await {
            eprintln!("Error handling client: {}", e);
        }
    });
    response
}

async fn handle_client(fut: upgrade::UpgradeFut) -> Result<(), WebSocketError> {
    let mut inner = fut.await?;
    inner.set_auto_close(true);
    inner.set_auto_pong(true);
    inner.set_writev(true);

    let mut ws = fastwebsockets::FragmentCollector::new(inner);
    loop {
        let frame = ws.read_frame().await?;
        match frame.opcode {
            OpCode::Close => break,
            OpCode::Text | OpCode::Binary => {
                let echo = Frame::new(true, frame.opcode, None, frame.payload);
                ws.write_frame(echo).await?;
            }
            _ => {}
        }
    }

    Ok(())
}
