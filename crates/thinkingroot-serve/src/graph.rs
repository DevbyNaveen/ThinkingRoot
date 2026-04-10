use axum::response::Html;

const GRAPH_HTML: &str = include_str!("graph.html");

pub async fn serve_graph() -> Html<&'static str> {
    Html(GRAPH_HTML)
}
