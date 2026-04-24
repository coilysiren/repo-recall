use axum::http::{StatusCode, Uri};
use axum::response::IntoResponse;
use maud::html;

use crate::routes::templates::{page, LINK, PANEL, PATH};

pub async fn not_found(uri: Uri) -> impl IntoResponse {
    let body = html! {
        section class=(PANEL) {
            h1 class="text-lg font-semibold mb-2" { "404 — not found" }
            p class="text-[#574f7d]/80 mb-1" { "No route matches " }
            p class=(PATH) { (uri.to_string()) }
            p class="mt-4" {
                a class=(LINK) href="/" { "← back to dashboard" }
            }
        }
    };
    (StatusCode::NOT_FOUND, page("not found", body))
}
