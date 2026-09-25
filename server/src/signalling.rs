//! HTTP signalling on loopback: `POST /offer` → complete answer, `GET /health`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use proto::signalling::{ErrorResponse, SessionDescription, HEALTH_BODY, HEALTH_PATH, OFFER_PATH};

use crate::session::{SessionError, SessionManager};

pub async fn serve(addr: SocketAddr, manager: Arc<SessionManager>) -> Result<()> {
    let app = Router::new()
        .route(OFFER_PATH, post(offer))
        .route(HEALTH_PATH, get(|| async { HEALTH_BODY }))
        .with_state(manager);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding signalling listener on {addr}"))?;
    tracing::info!(
        "signalling on http://{addr}{OFFER_PATH} (use: ssh -L {}:{addr} <vm>)",
        addr.port()
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server")?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    tracing::info!("shutdown requested");
}

async fn offer(
    State(manager): State<Arc<SessionManager>>,
    Json(desc): Json<SessionDescription>,
) -> Response {
    if !desc.is_offer() {
        return error(
            StatusCode::BAD_REQUEST,
            format!("expected type \"offer\", got \"{}\"", desc.kind),
        );
    }
    match manager.accept_offer(desc.sdp).await {
        Ok(sdp) => Json(SessionDescription::answer(sdp)).into_response(),
        Err(SessionError::BadOffer(msg)) => error(StatusCode::BAD_REQUEST, msg),
        Err(SessionError::Internal(e)) => {
            tracing::error!("offer failed: {e:#}");
            error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
        }
    }
}

fn error(status: StatusCode, message: String) -> Response {
    (status, Json(ErrorResponse { error: message })).into_response()
}
