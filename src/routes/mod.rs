mod galleries;
mod photos;
mod tags;

use axum::routing::{get, patch, post, put};
use axum::Router;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/photos", post(photos::upload).get(photos::list))
        .route("/api/photos/import-path", post(photos::import_path))
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
        .route("/api/galleries", get(galleries::list).post(galleries::create))
        .route(
            "/api/galleries/{id}",
            get(galleries::get_one).patch(galleries::update).delete(galleries::delete),
        )
        .route(
            "/api/galleries/{id}/photos",
            post(galleries::add_photos).delete(galleries::remove_photos),
        )
        .route("/api/galleries/{id}/order", put(galleries::reorder))
        .route(
            "/api/galleries/{id}/tags",
            post(galleries::add_tags).delete(galleries::remove_tags),
        )
        .route("/api/gallery-tags", get(galleries::tag_tree))
        .route(
            "/api/gallery-tags/{id}",
            patch(galleries::rename_tag).delete(galleries::delete_tag),
        )
}
