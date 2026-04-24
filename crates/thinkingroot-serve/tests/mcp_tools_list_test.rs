//! Phase D smoke test: the 4 new branch-management MCP tools must appear
//! in `tools/list`. Without this, agents can't discover them.

use thinkingroot_serve::mcp::tools;

#[tokio::test]
async fn tools_list_includes_branch_management_tools() {
    let resp = tools::handle_list(None).await;
    let v = serde_json::to_value(&resp).expect("serialize tools/list");
    let names: Vec<String> = v["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();

    for expected in [
        "list_branches",
        "delete_branch",
        "gc_branches",
        "rollback_merge",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "tools/list missing '{}'. got: {:?}",
            expected,
            names
        );
    }

    // Ensure pre-existing tools still advertised (guard against accidental removal).
    for existing in [
        "create_branch",
        "merge_branch",
        "checkout_branch",
        "diff_branch",
    ] {
        assert!(
            names.iter().any(|n| n == existing),
            "tools/list regression: '{}' missing. got: {:?}",
            existing,
            names
        );
    }
}
