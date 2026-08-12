mod photos;
mod tags;

use axum::routing::{get, patch, post};
use axum::Router;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/photos", post(photos::upload).get(photos::list))
        .route("/api/photos/by-hash/{hash}", get(photos::get_by_hash))
        .route("/api/photos/{id}", get(photos::get_one).delete(photos::delete_permanently))
        .route("/api/photos/{id}/file", get(photos::get_file))
        .route("/api/photos/{id}/thumbnail", get(photos::get_thumbnail))
        .route("/api/photos/{id}/regenerate-thumbnail", post(photos::regenerate_thumbnail))
        .route(
            "/api/photos/{id}/tags",
            post(photos::add_tags).delete(photos::remove_tags),
        )
        .route("/api/photos/tags", post(photos::bulk_add_tags))
        .route("/api/photos/bulk-delete", post(photos::bulk_delete_permanently))
        .route("/api/tags", get(tags::tree))
        .route("/api/tags/{id}", patch(tags::rename).delete(tags::delete))
}
