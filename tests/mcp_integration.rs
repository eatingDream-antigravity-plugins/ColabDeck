use colab_mcp::host::Host;
use colab_mcp::mcp::coordinator::ColabCoordinator;
use colab_mcp::mcp::server::{handle_rpc, Connection};
use colab_mcp::mcp::tools::{HtmlLoader, APP_MIME, APP_URI};
use colab_mcp::util::PrivateDir;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::Duration;

fn rpc(
    connection: &mut Connection,
    coord: &std::sync::Arc<ColabCoordinator>,
    loader: &HtmlLoader,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    handle_rpc(
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        connection,
        coord,
        loader,
    )
    .expect("RPC response")
}

#[test]
fn mcp_static_resource_delta_poll_and_full_mock_lifecycle() {
    let dir = PrivateDir::new(&std::env::temp_dir(), "mcp-e2e").unwrap();
    let coord = ColabCoordinator::with_host(dir.path().to_path_buf(), Some(true), Host::denied());
    let loader = HtmlLoader::new(None);
    let mut connection = Connection::default();
    // Coalesce with startup prewarm before checking initial demo state.
    coord.refresh(true).unwrap();

    let init = rpc(
        &mut connection,
        &coord,
        &loader,
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-11-25",
            "clientInfo": {"name": "fixture", "version": "0"},
            "capabilities": { "extensions": { "io.modelcontextprotocol/ui": { "mimeTypes": [APP_MIME] } } }
        }),
    );
    assert!(handle_rpc(
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        &mut connection,
        &coord,
        &loader
    )
    .is_none());
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    assert_eq!(
        init["result"]["capabilities"]["extensions"]["io.modelcontextprotocol/ui"]["mimeTypes"][0],
        APP_MIME
    );

    // 2. Verify tools/list visibility isolation and aux_pane + global entrypoints
    let tools = rpc(&mut connection, &coord, &loader, 2, "tools/list", json!({}));
    let list = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(list.len(), 10);
    let panel_tool = list.iter().find(|t| t["name"] == "colab_panel").unwrap();
    assert_eq!(panel_tool["_meta"]["ui"]["resourceUri"], APP_URI);
    let eps = panel_tool["_meta"]["com.google.antigravity/ui"]["entrypoints"]
        .as_array()
        .unwrap();
    assert!(eps.iter().any(|e| e["type"] == "aux_pane"));
    assert!(eps.iter().any(|e| e["type"] == "global"));
    let app_only_count = list
        .iter()
        .filter(|t| t["_meta"]["ui"]["visibility"] == json!(["app"]))
        .count();
    assert_eq!(app_only_count, 5);

    // 3. The fixed resource URI returns a static bundle without account state.
    let res = rpc(
        &mut connection,
        &coord,
        &loader,
        3,
        "resources/read",
        json!({ "uri": APP_URI }),
    );
    let html = res["result"]["contents"][0]["text"].as_str().unwrap();
    assert!(
        !html.contains("/*__COLAB_MCP_BOOT__*/"),
        "the static resource must replace its boot placeholder"
    );
    assert!(html.contains("__COLAB_BOOT__=[null][0]") || html.contains("__COLAB_BOOT__=null"));
    assert!(html.len() <= 65_536);
    assert!(!html.contains("c-g4-train"));

    // 4. Verify hash-dedup delta polling returns unchanged=true
    let p1 = rpc(
        &mut connection,
        &coord,
        &loader,
        4,
        "tools/call",
        json!({ "name": "colab_app_poll", "arguments": {} }),
    );
    let rev = p1["result"]["structuredContent"]["rev"].as_u64().unwrap();
    let p2 = rpc(
        &mut connection,
        &coord,
        &loader,
        5,
        "tools/call",
        json!({ "name": "colab_app_poll", "arguments": { "since_rev": rev } }),
    );
    assert_eq!(p2["result"]["structuredContent"]["unchanged"], true);

    // 5. Reap browser [?] orphan in demo mode, then provision A100 >= 70GB (40GB -> 80GB retry)
    let reap = rpc(
        &mut connection,
        &coord,
        &loader,
        6,
        "tools/call",
        json!({
            "name": "colab_session_action",
            "arguments": { "action": "reap_orphan", "endpoint": "m-s-demo-web9" }
        }),
    );
    assert_eq!(reap["result"]["structuredContent"]["released"], true);

    let prov = rpc(
        &mut connection,
        &coord,
        &loader,
        7,
        "tools/call",
        json!({
            "name": "colab_session_action",
            "arguments": {
                "action": "provision",
                "name": "a100-80",
                "accelerator": "A100",
                "min_vram_mib": 70000
            }
        }),
    );
    assert_eq!(prov["result"]["isError"], false);

    // Wait briefly for mock provision worker thread to finish
    for _ in 0..30 {
        let st = coord.snapshot_json(None);
        if st["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["display_name"] == "a100-80")
        {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let snap = coord.snapshot_json(None);
    let a100 = snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["display_name"] == "a100-80")
        .expect("a100-80 provisioned");
    assert_eq!(a100["vram_mib"], 81_920);
    assert_eq!(a100["watchdog"]["armed"], true);

    // 6. Tunnel port 8080 must be rejected; port 8000 must open cleanly
    let bad_tun = rpc(
        &mut connection,
        &coord,
        &loader,
        8,
        "tools/call",
        json!({
            "name": "colab_tunnel_action",
            "arguments": { "action": "open", "session": "a100-80", "local_port": 18080, "remote_port": 8080 }
        }),
    );
    assert_eq!(bad_tun["error"]["code"], -32602);

    let good_tun = rpc(
        &mut connection,
        &coord,
        &loader,
        9,
        "tools/call",
        json!({
            "name": "colab_tunnel_action",
            "arguments": { "action": "open", "session": "a100-80", "local_port": 18001, "remote_port": 8000, "health_path": "/health" }
        }),
    );
    assert_eq!(good_tun["result"]["isError"], false);

    // 7. HF GGUF search & VRAM fit preview + bundled llama-cpp deploy on existing session
    let fit = rpc(
        &mut connection,
        &coord,
        &loader,
        10,
        "tools/call",
        json!({
            "name": "colab_app_plugins",
            "arguments": { "action": "inspect_hf", "repo": "unsloth/Qwen3-32B-GGUF", "accelerator": "G4" }
        }),
    );
    assert_eq!(fit["result"]["structuredContent"]["fits"][0]["fits"], true);

    let dep = rpc(
        &mut connection,
        &coord,
        &loader,
        11,
        "tools/call",
        json!({
            "name": "colab_deploy_action",
            "arguments": {
                "action": "deploy",
                "plugin_id": "llama-cpp",
                "session": "a100-80",
                "stop_on_failure": null,
                "inputs": { "repo": "unsloth/Qwen3-32B-GGUF", "port": 8002 }
            }
        }),
    );
    assert_eq!(dep["result"]["isError"], false);
    for _ in 0..40 {
        let st = coord.snapshot_json(None);
        if st["plugin_runs"][0]["state"] == "ready" {
            break;
        }
        thread::sleep(Duration::from_millis(80));
    }
    let after_dep = coord.snapshot_json(None);
    assert_eq!(after_dep["plugin_runs"][0]["state"], "ready");
    assert_eq!(
        after_dep["plugin_runs"][0]["endpoints"][0]["local_url"],
        "http://127.0.0.1:8002"
    );

    // 8. Stop session closes tunnels cleanly
    let stop = rpc(
        &mut connection,
        &coord,
        &loader,
        12,
        "tools/call",
        json!({ "name": "colab_session_action", "arguments": { "action": "stop", "session": "a100-80" } }),
    );
    assert_eq!(stop["result"]["structuredContent"]["stopped"], true);
    let ended = &stop["result"]["structuredContent"]["snapshot"]["ended_sessions"][0];
    assert_eq!(ended["name"], "a100-80");
    assert_eq!(ended["hardware"], "A100");
    assert_eq!(ended["stop_reason"], "user_stop");
    assert!(ended["duration_secs"].is_u64());

    coord.shutdown();
}

#[test]
fn axiomatic_design_lower_triangular_module_dependency_invariant() {
    // Enforce MIT Axiomatic Design Level-1 strict lower-triangularity in source imports:
    // Layer i may only `use crate::<m>` where layer(m) <= i.
    let layers: &[(&str, usize)] = &[
        ("error", 0),
        ("util", 0),
        ("settings", 1),
        ("host", 2),
        ("accounts", 2),
        ("cli", 2),
        ("mock", 2),
        ("sessions", 3),
        ("provision", 3),
        ("usage", 4),
        ("guard", 5),
        ("watchdog", 5),
        ("tunnel", 6),
        ("quant", 7),
        ("hf", 7),
        ("plugins", 7),
    ];
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for &(module, my_layer) in layers {
        let path = src_root.join(format!("{module}.rs"));
        let text = fs::read_to_string(&path).expect("read module source");
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("use crate::") {
                let target = rest.split("::").next().unwrap_or("");
                if let Some(&(_, dep_layer)) = layers.iter().find(|&&(m, _)| m == target) {
                    assert!(
                        dep_layer <= my_layer,
                        "Upper-triangular coupling violation: {module} (L{my_layer}) imports {target} (L{dep_layer})"
                    );
                }
            }
        }
    }
}

fn init_connection(
    version: &str,
    extensions: Value,
    coord: &std::sync::Arc<ColabCoordinator>,
    html: &HtmlLoader,
) -> Connection {
    let mut conn = Connection::default();
    let result = handle_rpc(
        &json!({"jsonrpc":"2.0","id":"i","method":"initialize","params":{
            "protocolVersion":version,"capabilities":{"extensions":extensions},
            "clientInfo":{"name":"fixture","version":"0"}
        }}),
        &mut conn,
        coord,
        html,
    )
    .unwrap();
    assert_eq!(result["result"]["protocolVersion"], version);
    assert!(handle_rpc(
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        &mut conn,
        coord,
        html
    )
    .is_none());
    conn
}

#[test]
fn session_snapshot_and_switch_preview_include_unmanaged_history() {
    let data = PrivateDir::new(&std::env::temp_dir(), "snapshot-history").unwrap();
    let history_dir = data.path().join("fixture-history");
    fs::create_dir(&history_dir).unwrap();
    fs::write(
        history_dir.join("External _Run.jsonl"),
        r#"{"timestamp":"1970-01-02T00:00:00Z","event_type":"session_created","endpoint":"m-s-demo-web9"}"#,
    ).unwrap();
    let coord = ColabCoordinator::with_history_dir(
        data.path().to_owned(),
        Some(true),
        Host::denied(),
        Some(history_dir),
    );
    coord.refresh(true).unwrap();
    let tunnel = coord.open_tunnel("c-g4-train", 18200, 8000, None).unwrap();
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let mut call = |id, name, args| {
        rpc(
            &mut conn,
            &coord,
            &html,
            id,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )
    };
    let poll = call(1, "colab_app_poll", json!({}));
    let snapshot = &poll["result"]["structuredContent"];
    let sessions = snapshot["sessions"].as_array().unwrap();
    let orphan = sessions
        .iter()
        .find(|row| row["is_orphan"] == true)
        .unwrap();
    let tracked = sessions
        .iter()
        .find(|row| row["is_orphan"] == false)
        .unwrap();
    assert_eq!(tracked["start_source"], "tracked");
    assert!(tracked["external_name"].is_null());
    assert_eq!(orphan["display_name"], "[?]");
    assert_eq!(orphan["start_source"], "cli_history");
    assert_eq!(orphan["external_name"], "External_Run");
    assert_eq!(orphan["started_at_epoch"], 86_400);
    assert_eq!(
        orphan["uptime_secs"],
        snapshot["sampled_at_epoch"].as_u64().unwrap() - 86_400
    );
    assert!(orphan["spend_cu"].as_f64().is_some_and(|spend| spend > 0.0));
    assert_eq!(orphan["spend_estimated"], true);
    assert!(orphan["watchdog"].is_null());
    assert!(!snapshot.to_string().contains("fixture-history"));
    assert_eq!(snapshot["hardware_rates"].as_object().unwrap().len(), 8);
    assert_eq!(
        snapshot["hardware_rates"]["G4"],
        json!({"cu_per_hour":8.90,"measured":false,"high_mem_cu_per_hour":8.90,"high_mem_measured":false})
    );

    let response = call(2, "colab_app_accounts", json!({"action":"preview_switch"}));
    let impact = &response["result"]["structuredContent"];
    assert_eq!(impact["active_sessions"], 2);
    assert_eq!(impact["open_tunnels"], 1);
    assert_eq!(
        impact["closing_tunnels"],
        json!([{
            "id": tunnel.id, "local_url": tunnel.local_url, "session": "c-g4-train", "remote_port": 8000,
        }])
    );
    assert_eq!(
        impact["watched_sessions"][0]["endpoint"],
        tracked["endpoint"]
    );
    assert!(impact["watched_sessions"][0]["remaining_secs"].is_u64());
    assert_eq!(impact["unwatched_sessions"], json!([]));
    assert_eq!(
        impact["orphan_sessions"],
        json!([{
            "endpoint": orphan["endpoint"], "hardware": "T4", "external_name": "External_Run",
        }])
    );

    let dismiss = json!({"action":"dismiss","tunnel_id":tunnel.id});
    let active = call(3, "colab_tunnel_action", dismiss.clone());
    assert_eq!(active["result"]["structuredContent"]["dismissed"], false);
    assert!(coord.close_tunnel(tunnel.id));
    let stopped = call(4, "colab_tunnel_action", dismiss);
    assert_eq!(stopped["result"]["structuredContent"]["dismissed"], true);
    assert_eq!(
        stopped["result"]["structuredContent"]["snapshot"]["tunnels"],
        json!([])
    );
    coord.shutdown();
}

#[test]
fn protocol_versions_tool_permissions_and_strict_envelopes() {
    let temp = PrivateDir::new(&std::env::temp_dir(), "rpc").unwrap();
    let coord = ColabCoordinator::with_host(temp.path().to_owned(), Some(true), Host::denied());
    let html = HtmlLoader::new(None);

    let mut rejected = Connection::default();
    for invalid in [
        json!({"protocolVersion":"2025-11-25","capabilities":{}}),
        json!({"protocolVersion":"2025-11-25","clientInfo":{"name":"fixture","version":"0"},
            "capabilities":{"extensions":{"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME, 7]}}}}),
    ] {
        let reply = rpc(&mut rejected, &coord, &html, 1, "initialize", invalid);
        assert_eq!(reply["error"]["code"], -32602);
    }

    for extensions in [
        json!({}),
        json!({"io.modelcontextprotocol/ui": {"mimeTypes": ["text/html"]}}),
    ] {
        let mut conn = init_connection("2025-11-25", extensions, &coord, &html);
        let list = rpc(&mut conn, &coord, &html, 2, "tools/list", json!({}));
        let tools = list["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 5);
        assert!(tools.iter().all(|t| t.get("_meta").is_none()));
        for tool in [
            "colab_app_poll",
            "colab_app_accounts",
            "colab_app_settings",
            "colab_app_plugins",
            "colab_app_watchdog",
            "missing",
        ] {
            let result = rpc(
                &mut conn,
                &coord,
                &html,
                3,
                "tools/call",
                json!({"name": tool}),
            );
            assert_eq!(result["error"]["code"], -32602);
        }
        let reply = rpc(
            &mut conn,
            &coord,
            &html,
            4,
            "resources/read",
            json!({"uri":"ui://missing"}),
        );
        assert_eq!(reply["error"]["code"], -32002);
        assert_eq!(
            rpc(&mut conn, &coord, &html, 5, "unknown", json!({}))["error"]["code"],
            -32601
        );
    }
    for version in ["2025-03-26", "2025-06-18", "2025-11-25"] {
        let mut conn = init_connection(version, json!({}), &coord, &html);
        let mut run = |msg| handle_rpc(&msg, &mut conn, &coord, &html);
        for bad in [
            json!({}),
            json!({"jsonrpc":"1.0","method":"ping","id":1}),
            json!({"jsonrpc":"2.0","method":"ping","id":true}),
            json!({"jsonrpc":"2.0","method":"ping","id":null}),
            json!({"jsonrpc":"2.0","method":"ping","id":1.5}),
            json!({"jsonrpc":"2.0","method":"ping","params":null}),
            json!("ping"),
            json!([]),
            json!([{"jsonrpc":"2.0","method":"initialize","id":1}]),
        ] {
            let reply = run(bad).unwrap();
            assert_eq!(reply["error"]["code"], -32600);
        }
        let notice =
            json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":123}});
        let pending = run(json!([notice.clone()]));
        let mixed = run(
            json!([notice, {"jsonrpc":"2.0","method":"ping","id":"a"}, false,
            {"jsonrpc":"2.0","method":"ping","id":null}]),
        )
        .unwrap();
        if version == "2025-03-26" {
            assert!(pending.is_none());
            assert_eq!(mixed.as_array().unwrap().len(), 3);
            assert_eq!(mixed[0]["id"], "a");
            assert_eq!(mixed[1]["error"]["code"], -32600);
            assert_eq!(mixed[2]["error"]["code"], -32600);
            assert!(mixed[2]["id"].is_null());
        } else {
            assert_eq!(pending.unwrap()["error"]["code"], -32600);
            assert_eq!(mixed["error"]["code"], -32600);
        }
    }
    coord.shutdown();
}

#[test]
fn legacy_batches_limit_item_count_and_serialized_replies() {
    let tmp = PrivateDir::new(&std::env::temp_dir(), "batch-budget").unwrap();
    let coord = ColabCoordinator::with_host(tmp.path().to_owned(), Some(true), Host::denied());
    let path = tmp.path().join("fixture.html");
    fs::write(
        &path,
        format!("/*__COLAB_MCP_BOOT__*/{}", "x".repeat(900 * 1024)),
    )
    .unwrap();
    let html = HtmlLoader::new(Some(path));
    let mut conn = init_connection("2025-03-26", json!({}), &coord, &html);
    let batch = |count| {
        Value::Array((0..count).map(|id| json!({
        "jsonrpc":"2.0", "method":"resources/read", "id":id, "params":{"uri":APP_URI}
    })).collect())
    };
    let small = handle_rpc(&batch(4), &mut conn, &coord, &html).unwrap();
    assert_eq!(small.as_array().unwrap().len(), 4);
    let large = handle_rpc(&batch(5), &mut conn, &coord, &html).unwrap();
    assert_eq!(large["error"]["code"], -32003);
    let oversize = handle_rpc(&batch(65), &mut conn, &coord, &html).unwrap();
    assert_eq!(oversize["error"]["code"], -32600);
    coord.shutdown();
}

#[test]
fn forced_mock_rejects_unsafe_actions_and_preserves_the_real_ledger() {
    use colab_mcp::accounts::ADC_ACCOUNT_ID;
    use colab_mcp::host::{ExecRequest, LineAction};
    use colab_mcp::sessions::{
        LedgerData, PendingStop, SessionLedger, SpendMeterState, StopReason, TrackedSession,
    };
    use colab_mcp::settings::{Accelerator, GuardPolicy};
    use colab_mcp::util::CancelToken;

    let temp = PrivateDir::new(&std::env::temp_dir(), "isolation").unwrap();
    let dir = temp.path();
    let host = Host::denied();
    for program in ["colab", "ssh", "ssh-keygen", "git", "curl"] {
        let error = host
            .run(
                ExecRequest::new(program, Vec::new()),
                &CancelToken::new(),
                &mut |_| LineAction::Continue,
            )
            .unwrap_err();
        assert!(error.message.contains("execution is disabled"));
    }

    let ledger = LedgerData {
        sessions: vec![TrackedSession {
            name: "keep-real".into(),
            account_id: ADC_ACCOUNT_ID.into(),
            endpoint: Some("real-endpoint".into()),
            generation: 0,
            allocation: None,
            is_orphan: false,
            hardware: None,
            accelerator: Accelerator::G4,
            high_mem: false,
            gpu: None,
            vram_mib: None,
            policy: GuardPolicy::default(),
            spend: SpendMeterState::new(123),
            watchdog: None,
            stopped: None,
            pending_stop: Some(PendingStop::new(StopReason::User)),
            measured_rate_cu_per_hour: None,
            below_spec: false,
        }],
        ..LedgerData::default()
    };
    let store = SessionLedger::new(dir);
    store.save(&ledger).unwrap();
    let read_real = || fs::read(dir.join("ledger.json")).unwrap();
    let original = read_real();
    let coord = ColabCoordinator::with_host(dir.to_path_buf(), Some(true), host.clone());
    coord.refresh(true).unwrap();
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let mut tool = |name, args| {
        rpc(
            &mut conn,
            &coord,
            &html,
            2,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )
    };
    for action in ["remove", "install_git", "install_dir", "list", "cancel_run"] {
        let reply = tool(
            "colab_deploy_action",
            json!({"action":action,"plugin_id":""}),
        );
        assert_eq!(reply["error"]["code"], -32602);
    }
    for colab_path in [json!(null), json!("/tmp/other-program")] {
        let reply = tool(
            "colab_app_settings",
            json!({"action":"update","patch":{"colab_path":colab_path}}),
        );
        assert_eq!(reply["result"]["isError"], true);
    }
    let reply = tool(
        "colab_app_settings",
        json!({"action":"update","patch":{"locale":"en"}}),
    );
    assert_eq!(
        reply["result"]["structuredContent"]["settings"]["backend_mode"],
        "mock"
    );
    let reply = tool(
        "colab_app_settings",
        json!({"action":"update","patch":{"backend_mode":"real"}}),
    );
    assert_eq!(reply["result"]["isError"], true);
    let synthetic = ["hf_", "1234567890", "ABCDEFGHIJKLMN"].concat();
    let saved = tool(
        "colab_app_accounts",
        json!({"action":"set_hf_token","token":synthetic}),
    );
    assert_eq!(
        saved["result"]["structuredContent"]["snapshot"]["hf_token_set"],
        true
    );
    let account = tool(
        "colab_app_accounts",
        json!({"action":"add","account_id":"redacted","label":synthetic,"auth_mode":"adc"}),
    );
    assert!(!account.to_string().contains(&synthetic));
    let snap = tool("colab_app_poll", json!({}));
    assert!(!snap.to_string().contains(&synthetic));
    assert_eq!(snap["result"]["structuredContent"]["hf_token_set"], true);
    let reply = tool(
        "colab_app_plugins",
        json!({"action":"install_git","url":"https://example.test/project.git"}),
    );
    assert_eq!(reply["result"]["isError"], true);
    assert!(reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("requires real backend"));
    for args in [
        json!({"action":"provision","accelerator":"UNKNOWN"}),
        json!({"action":"provision","min_balance_cu":"invalid"}),
        json!({"action":"provision","max_runtime_secs":-1}),
    ] {
        assert_eq!(tool("colab_session_action", args)["error"]["code"], -32602);
    }
    for port in [0, 1_023, 65_536, 73_536] {
        let reply = tool(
            "colab_tunnel_action",
            json!({"action":"open","session":"c-g4-train","local_port":port}),
        );
        assert_eq!(reply["error"]["code"], -32602);
    }
    assert_eq!(
        coord
            .cached_snapshot()
            .json(None, std::time::Instant::now())["backend_mode"],
        "mock"
    );
    coord.shutdown();
    assert_eq!(read_real(), original);

    let real = ColabCoordinator::with_host(dir.to_path_buf(), Some(false), host);
    assert_eq!(
        real.snapshot_json(None)["pending_stops"][0]["endpoint"],
        "real-endpoint"
    );
    real.shutdown();
    assert!(store.load().unwrap().sessions[0].stopped.is_none());
}

#[test]
fn html_override_fails_explicitly_and_detects_replacement() {
    let temp = PrivateDir::new(&std::env::temp_dir(), "html").unwrap();
    let path = temp.path().join("panel.html");
    let loader = HtmlLoader::new(Some(path.clone()));
    let error = || loader.load().err().expect("expected an HTML load error");
    assert!(error().contains("panel.html"));
    fs::write(&path, "").unwrap();
    assert!(error().contains("empty"));
    fs::write(&path, "<html>missing the placeholder</html>").unwrap();
    assert!(error().contains("placeholder"));
    fs::write(&path, vec![b'x'; 1_048_577]).unwrap();
    assert!(error().contains("1 MiB"));
    fs::write(&path, [0xff]).unwrap();
    assert!(error().contains("UTF-8"));
    fs::write(&path, "<script>let boot=/*__COLAB_MCP_BOOT__*/</script>A").unwrap();
    assert!(loader.load().unwrap().render_with_boot("{}").ends_with('A'));
    let replacement = temp.path().join("replace.html");
    fs::write(
        &replacement,
        "<script>let boot=/*__COLAB_MCP_BOOT__*/</script>B",
    )
    .unwrap();
    fs::rename(replacement, path).unwrap();
    assert!(loader.load().unwrap().render_with_boot("{}").ends_with('B'));
}

#[test]
fn deployment_reserves_quota_and_cancel_releases_its_original_account_and_tunnel() {
    use colab_mcp::accounts::{AccountProfile, ADC_ACCOUNT_ID};
    use colab_mcp::mcp::coordinator::ProvisionRequest;
    use colab_mcp::settings::{Accelerator, AuthMode};
    use colab_mcp::util::now_secs;
    use std::collections::BTreeMap;

    let temp = PrivateDir::new(&std::env::temp_dir(), "cancel-run").unwrap();
    let coord = ColabCoordinator::with_host(temp.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    coord.reap_orphan("m-s-demo-web9").unwrap();
    let demo = coord
        .deploy_plugin(
            "llama-cpp",
            None,
            Some(Accelerator::G4),
            BTreeMap::from([
                ("repo".into(), json!("unsloth/Qwen3-32B-GGUF")),
                ("port".into(), json!(18088)),
            ]),
        )
        .unwrap();
    let provision: ProvisionRequest =
        serde_json::from_value(json!({"name":"over-quota","accelerator":"G4"})).unwrap();
    assert_eq!(
        coord.start_provision(provision).unwrap_err().kind,
        colab_mcp::error::ErrorKind::SessionLimitReached
    );
    coord
        .add_account(AccountProfile {
            id: "other".into(),
            label: "Other".into(),
            email: None,
            auth_mode: AuthMode::Adc,
            added_at: now_secs(),
        })
        .unwrap();
    coord.switch_account("other").unwrap();
    thread::sleep(Duration::from_millis(900));
    coord.switch_account(ADC_ACCOUNT_ID).unwrap();
    for _ in 0..200 {
        if coord.snapshot_json(None)["plugin_runs"][0]["state"] == "ready" {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let snap = coord.snapshot_json(None);
    assert_eq!(snap["plugin_runs"][0]["state"], "ready");
    let tunnel = &snap["tunnels"][0];
    assert_eq!(tunnel["spec"]["account_id"], ADC_ACCOUNT_ID);
    assert!(tunnel["spec"]["endpoint"].is_string());
    coord.switch_account("other").unwrap();
    let html = HtmlLoader::new(None);
    let mut connection = init_connection("2025-11-25", json!({}), &coord, &html);
    let rejected = rpc(
        &mut connection,
        &coord,
        &html,
        1,
        "tools/call",
        json!({
            "name":"colab_deploy_action","arguments":{
                "action":"cancel","run_id":demo.run_id,"expected_backend_mode":"mock","account_id":"other"
            }
        }),
    );
    assert_eq!(
        rejected["result"]["structuredContent"]["error"],
        "account mismatch; refresh panel"
    );
    assert!(coord.cancel_run(&demo.run_id));

    coord.switch_account(ADC_ACCOUNT_ID).unwrap();
    for _ in 0..60 {
        if coord.snapshot_json(None)["plugin_runs"][0]["state"] == "cancelled" {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let snap = coord.snapshot_json(None);
    assert_eq!(snap["plugin_runs"][0]["state"], "cancelled");
    assert_eq!(snap["tunnels"][0]["state"]["state"], "stopped");
    assert!(snap["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["display_name"] != demo.session));
    assert!(snap["pending_stops"].as_array().unwrap().is_empty());
    coord.shutdown();
}

#[test]
fn public_snapshots_hide_account_details_and_app_polls_can_upgrade_them() {
    use colab_mcp::accounts::{AccountProfile, AccountStore};
    use colab_mcp::settings::AuthMode;
    let root = PrivateDir::new(&std::env::temp_dir(), "account-privacy").unwrap();
    let store = AccountStore::new(root.path());
    let email = ["fixture", "@", "example.invalid"].concat();
    store
        .upsert(
            AccountProfile {
                id: "fixture".into(),
                label: "Fixture label".into(),
                email: Some(email.clone()),
                auth_mode: AuthMode::Adc,
                added_at: 0,
            },
            true,
        )
        .unwrap();
    let registry_file = root.path().join("accounts.json");
    let original = fs::read(&registry_file).unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let mut id = 0;
    let mut tool = |name: &str, args: Value| {
        id += 1;
        rpc(
            &mut conn,
            &coord,
            &html,
            id,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )["result"]
            .clone()
    };
    for tool_name in ["colab_panel", "colab_status"] {
        let public = tool(tool_name, json!({}));
        assert!(!public.to_string().contains(&email));
        assert_eq!(
            public["structuredContent"]["active_account"]["label"],
            "Fixture label"
        );
        let app = tool(
            "colab_app_poll",
            json!({"since_rev": public["structuredContent"]["rev"]}),
        );
        assert_eq!(app["structuredContent"]["unchanged"], false);
        assert_eq!(app["structuredContent"]["active_account"]["email"], email);
    }

    for args in [
        json!({"action":"switch","account_id":"adc-default"}),
        json!({"action":"switch","account_id":"adc-default","confirm":false}),
    ] {
        assert_eq!(tool("colab_app_accounts", args)["isError"], true);
    }
    let switch = tool(
        "colab_app_accounts",
        json!({"action":"switch","account_id":"adc-default","confirm":true}),
    );
    assert_eq!(
        switch["structuredContent"]["snapshot"]["active_account"]["id"],
        "adc-default"
    );
    let removed = tool(
        "colab_app_accounts",
        json!({"action":"remove","account_id":"fixture"}),
    );
    assert_eq!(removed["isError"], false);
    assert_eq!(fs::read(&registry_file).unwrap(), original);
    coord.shutdown();
}

#[test]
fn real_account_switch_rejects_leaving_a_billing_oauth_profile() {
    use colab_mcp::accounts::{AccountProfile, AccountStore, ADC_ACCOUNT_ID};
    use colab_mcp::sessions::{LedgerData, SessionLedger, SpendMeterState, TrackedSession};
    use colab_mcp::settings::{Accelerator, AuthMode, GuardPolicy};
    let root = PrivateDir::new(&std::env::temp_dir(), "oauth-switch").unwrap();
    let accounts = AccountStore::new(root.path());
    let profile = "oauth-fixture";
    accounts
        .upsert(
            AccountProfile {
                id: profile.into(),
                label: "OAuth fixture".into(),
                email: None,
                auth_mode: AuthMode::OAuth2,
                added_at: 0,
            },
            true,
        )
        .unwrap();
    let mut ledger = LedgerData::default();
    ledger.record_session(TrackedSession {
        name: "oauth-runtime".into(),
        account_id: profile.into(),
        endpoint: Some("fixture-endpoint".into()),
        generation: 0,
        allocation: None,
        is_orphan: false,
        hardware: None,
        accelerator: Accelerator::G4,
        high_mem: false,
        gpu: None,
        vram_mib: None,
        policy: GuardPolicy::default(),
        spend: SpendMeterState::new(colab_mcp::util::now_secs()),
        watchdog: None,
        stopped: None,
        pending_stop: None,
        measured_rate_cu_per_hour: None,
        below_spec: false,
    });
    SessionLedger::new(root.path()).save(&ledger).unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(false), Host::denied());
    let blocked = coord.switch_account(ADC_ACCOUNT_ID).unwrap_err();
    assert!(blocked.message.contains("stop active OAuth2 runtimes"));
    let blocked = coord.remove_account(profile).unwrap_err();
    assert!(blocked.message.contains("stop active OAuth2 runtimes"));
    assert_eq!(accounts.load().unwrap().active_id, profile);

    ledger.mark_stopped(
        profile,
        "oauth-runtime",
        colab_mcp::sessions::StopReason::User,
        colab_mcp::util::now_secs(),
    );
    SessionLedger::new(root.path()).save(&ledger).unwrap();
    let unverified = coord.remove_account(profile).unwrap_err();
    assert!(unverified.message.contains("execution is disabled"));
    assert_eq!(accounts.load().unwrap().active_id, profile);
    coord.shutdown();
}

#[test]
fn replaced_endpoints_and_mismatched_accounts_are_rejected() {
    let root = PrivateDir::new(&std::env::temp_dir(), "pin-target").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let mut id = 0;
    let mut tool = |name: &str, args: Value| {
        id += 1;
        rpc(
            &mut conn,
            &coord,
            &html,
            id,
            "tools/call",
            json!({"name":name,"arguments":args}),
        )["result"]
            .clone()
    };
    for action in ["stop", "restart_kernel", "url", "reap_orphan"] {
        let stale = tool(
            "colab_session_action",
            json!({
                "action":action, "session":"c-g4-train", "endpoint":"m-s-demo-web9", "account_id":"adc-default"
            }),
        );
        assert_eq!(stale["isError"], true);
        assert!(stale["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("replaced"));
        let wrong_account = tool(
            "colab_session_action",
            json!({
                "action":action, "session":"c-g4-train", "endpoint":null, "account_id":"stale-account"
            }),
        );
        assert_eq!(
            wrong_account["structuredContent"]["error"],
            "account mismatch; refresh panel"
        );
    }
    for action in ["arm", "extend", "cancel"] {
        let stale = tool(
            "colab_app_watchdog",
            json!({
                "action":action, "session":"c-g4-train", "endpoint":"m-s-demo-web9", "account_id":"adc-default"
            }),
        );
        assert_eq!(stale["isError"], true);
    }
    for (name, mut args) in [
        (
            "colab_session_action",
            json!({"action":"provision","name":"stale"}),
        ),
        (
            "colab_session_action",
            json!({"action":"cancel_provision","job_id":"stale"}),
        ),
        (
            "colab_session_action",
            json!({"action":"stop","session":"c-g4-train"}),
        ),
        (
            "colab_session_action",
            json!({"action":"reap_orphan","endpoint":"m-s-demo-web9"}),
        ),
        (
            "colab_session_action",
            json!({"action":"restart_kernel","session":"c-g4-train"}),
        ),
        (
            "colab_session_action",
            json!({"action":"url","session":"c-g4-train"}),
        ),
        (
            "colab_deploy_action",
            json!({"action":"deploy","inputs":{"repo":"fixture/repo"}}),
        ),
        (
            "colab_deploy_action",
            json!({"action":"cancel","run_id":"stale"}),
        ),
        (
            "colab_app_plugins",
            json!({"action":"deploy","inputs":{"repo":"fixture/repo"}}),
        ),
        (
            "colab_app_plugins",
            json!({"action":"cancel_run","run_id":"stale"}),
        ),
        (
            "colab_app_watchdog",
            json!({"action":"cancel","session":"c-g4-train"}),
        ),
    ] {
        args["expected_backend_mode"] = json!("real");
        args["account_id"] = json!("adc-default");
        let stale = tool(name, args.clone());
        assert_eq!(
            stale["structuredContent"]["error"],
            "backend mode changed; refresh panel"
        );
        args["expected_backend_mode"] = json!("mock");
        args["account_id"] = json!("stale-account");
        let wrong_account = tool(name, args);
        assert_eq!(
            wrong_account["structuredContent"]["error"],
            "account mismatch; refresh panel"
        );
    }
    let url = tool(
        "colab_session_action",
        json!({
            "action":"url","session":"c-g4-train","endpoint":null,"account_id":null,"expected_backend_mode":null
        }),
    );
    assert_eq!(url["isError"], false);
    assert_eq!(
        coord.snapshot_json(None)["sessions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    coord.shutdown();
}

#[test]
fn a_provision_can_explicitly_clear_guard_and_vram_defaults() {
    let root = PrivateDir::new(&std::env::temp_dir(), "clear-guards").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    assert!(coord.reap_orphan("m-s-demo-web9").unwrap());
    coord
        .update_settings(
            serde_json::from_value(json!({"accelerator":"T4", "min_vram_mib": 70_000})).unwrap(),
        )
        .unwrap();
    let req = serde_json::from_value(json!({
        "name":"no-guard", "max_runtime_secs":0, "idle_timeout_secs":null,
        "max_spend_cu":0.0, "min_balance_cu":null, "min_vram_mib":null
    }))
    .unwrap();
    let job = coord.start_provision(req).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let spec = loop {
        let snap = coord.snapshot_json(None);
        if snap["provision_jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["job_id"] == job.job_id && row["done"] == true)
        {
            assert!(snap["provision_jobs"][0]["error"].is_null());
            break snap["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["display_name"] == "no-guard")
                .unwrap()
                .clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "mock provision did not complete"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(spec["watchdog"].is_null());
    assert!(spec["vram_mib"].as_u64().is_some_and(|vram| vram < 70_000));
    let changed = coord.update_settings(serde_json::from_value(json!({
        "max_runtime_secs":null, "idle_timeout_secs":0, "max_spend_cu":0, "min_balance_cu":null
    })).unwrap()).unwrap();
    assert_eq!(
        changed.defaults.guard,
        colab_mcp::settings::GuardPolicy::unlimited()
    );
    assert!(coord
        .update_settings(serde_json::from_value(json!({"max_spend_cu":-1})).unwrap())
        .is_err());
    coord.shutdown();
}

#[test]
fn deploy_rejects_invalid_inputs_before_provisioning_and_waits_for_required_tunnel() {
    use std::collections::BTreeMap;
    let root = PrivateDir::new(&std::env::temp_dir(), "deploy-inputs").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    let as_inputs = |value| serde_json::from_value::<BTreeMap<String, Value>>(value).unwrap();
    for values in [
        json!({}),
        json!({"repo":"", "port":"8201"}),
        json!({"repo":"fixture/repo", "port":true}),
        json!({"repo":"fixture/repo", "unexpected":1}),
        json!({"repo":"fixture/repo", "alias":"invalid\u{0}"}),
        json!({"repo":"fixture/repo", "api_key":"invalid\n"}),
        json!({"repo":"fixture/repo", "api_key":"invalid\u{7f}"}),
    ] {
        let err = coord
            .deploy_plugin("llama-cpp", None, None, as_inputs(values))
            .unwrap_err();
        assert_eq!(err.kind, colab_mcp::error::ErrorKind::InvalidInput);
    }
    let cpu = coord
        .deploy_plugin(
            "llama-cpp",
            None,
            Some(colab_mcp::settings::Accelerator::None),
            as_inputs(json!({"repo":"fixture/repo"})),
        )
        .unwrap_err();
    assert!(cpu.message.contains("requires a GPU"));
    let snapshot = coord.snapshot_json(None);
    assert!(snapshot["plugin_runs"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["sessions"].as_array().unwrap().len(), 2);
    let manual = coord.open_tunnel("c-g4-train", 8201, 8201, None).unwrap();
    let run = coord
        .deploy_plugin(
            "llama-cpp",
            Some("c-g4-train".into()),
            None,
            as_inputs(json!({"repo":"fixture/repo", "port":"8201", "api_key":"", "alias":""})),
        )
        .unwrap();
    let failed = await_run(&coord, &run.run_id, "failed");
    assert_eq!(failed["created"], false);
    assert_eq!(failed["endpoint"], "m-s-demo-g401");
    assert!(failed["message"]
        .as_str()
        .unwrap()
        .contains("required tunnel"));
    let state = coord.snapshot_json(None);
    assert!(state["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["display_name"] == "c-g4-train"));
    assert!(state["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tunnel| tunnel["id"] == manual.id && tunnel["state"]["state"] == "up"));
    coord.shutdown();
}

#[test]
fn failed_deploy_releases_only_a_new_runtime_when_requested() {
    let root = PrivateDir::new(&std::env::temp_dir(), "deploy-failure").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    assert!(coord.reap_orphan("m-s-demo-web9").unwrap());
    let port = 8208;
    let manual = coord.open_tunnel("c-g4-train", port, port, None).unwrap();
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    for (id, tool, target, stop_on_failure, should_release) in [
        (1, "colab_app_plugins", None, true, true),
        (2, "colab_deploy_action", None, false, false),
        (3, "colab_deploy_action", Some("c-g4-train"), true, false),
    ] {
        let response = rpc(
            &mut conn,
            &coord,
            &html,
            id,
            "tools/call",
            json!({
                "name":tool, "arguments":{
                    "action":"deploy", "session":target, "stop_on_failure":stop_on_failure,
                    "inputs":{"repo":"fixture/repo", "port":port},
                }
            }),
        );
        assert_eq!(response["result"]["isError"], false);
        let run = &response["result"]["structuredContent"]["run"];
        let failed = await_run(&coord, run["run_id"].as_str().unwrap(), "failed");
        assert!(failed["message"]
            .as_str()
            .unwrap()
            .contains("required tunnel"));
        coord.refresh(true).unwrap();
        let snapshot = coord.snapshot_json(None);
        let live = snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|session| session["display_name"] == run["session"]);
        assert_eq!(live, !should_release);
        assert_eq!(snapshot["pending_stops"], json!([]));
        assert!(snapshot["tunnels"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tunnel| tunnel["id"] == manual.id && tunnel["state"]["state"] == "up"));
    }
    coord.shutdown();
}

fn await_run(coord: &ColabCoordinator, id: &str, expected: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(4);
    loop {
        let snapshot = coord.snapshot_json(None);
        if let Some(run) = snapshot["plugin_runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|run| run["run_id"] == id)
        {
            if run["state"] == expected {
                return run.clone();
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "deployment did not reach {expected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn cancelling_a_shared_deployment_keeps_the_runtime_until_its_last_user_leaves() {
    let root = PrivateDir::new(&std::env::temp_dir(), "shared-deploy").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    coord.refresh(true).unwrap();
    assert!(coord.reap_orphan("m-s-demo-web9").unwrap());
    let deploy = |session, port| {
        coord
            .deploy_plugin(
                "llama-cpp",
                session,
                None,
                serde_json::from_value(json!({"repo":"fixture/repo","port":port})).unwrap(),
            )
            .unwrap()
    };
    let first = deploy(None, 8202);
    await_run(&coord, &first.run_id, "ready");
    let second = deploy(Some(first.session.clone()), 8203);
    await_run(&coord, &second.run_id, "ready");

    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let response = rpc(
        &mut conn,
        &coord,
        &html,
        2,
        "tools/call",
        json!({
            "name":"colab_app_plugins", "arguments":{"action":"cancel_run","run_id":first.run_id}
        }),
    );
    assert!(
        response["result"]["structuredContent"]["snapshot"]["plugin_runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["run_id"] == first.run_id && run["state"] == "cancelling")
    );
    await_run(&coord, &first.run_id, "cancelled");
    await_run(&coord, &second.run_id, "ready");
    let shared = coord.snapshot_json(None);
    assert!(shared["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|s| s["display_name"] == first.session));
    assert!(shared["tunnels"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tunnel| tunnel["spec"]["remote_port"] == 8203 && tunnel["state"]["state"] == "up"));
    assert!(coord.cancel_run(&second.run_id));
    await_run(&coord, &second.run_id, "cancelled");
    assert!(coord.snapshot_json(None)["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s["display_name"] != first.session));
    coord.shutdown();
}

#[test]
fn mcp_tool_names_stay_within_64_char_host_prefix_limit() {
    let root = PrivateDir::new(&std::env::temp_dir(), "tool-len").unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), Some(true), Host::denied());
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );
    let listed = rpc(&mut conn, &coord, &html, 2, "tools/list", json!({}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(!tools.is_empty());
    let prefix_len = "colab-mcp_colab-mcp_".len();
    for tool in tools {
        let name = tool["name"].as_str().unwrap();
        assert!(
            prefix_len + name.len() <= 64,
            "tool {name} exceeds 64-char host prefix budget ({} > 64)",
            prefix_len + name.len()
        );
    }
    coord.shutdown();
}

#[test]
fn settings_hot_reload_and_structured_error_payloads_work_end_to_end() {
    let root = PrivateDir::new(&std::env::temp_dir(), "hot-reload-e2e").unwrap();
    let settings_file = root.path().join("settings.json");
    fs::write(
        &settings_file,
        r#"{"backend_mode":"mock","locale":"zh-CN"}"#,
    )
    .unwrap();
    let coord = ColabCoordinator::with_host(root.path().to_owned(), None, Host::denied());
    let html = HtmlLoader::new(None);
    let mut conn = init_connection(
        "2025-11-25",
        json!({"io.modelcontextprotocol/ui":{"mimeTypes":[APP_MIME]}}),
        &coord,
        &html,
    );

    // Trigger initial poll and verify settings_path + 7-day seeded demo balance history
    let p0 = rpc(
        &mut conn,
        &coord,
        &html,
        2,
        "tools/call",
        json!({ "name": "colab_app_poll", "arguments": {} }),
    );
    let initial = &p0["result"]["structuredContent"];
    assert!(initial["settings_path"]
        .as_str()
        .unwrap()
        .ends_with("settings.json"));
    assert!(initial["balance_history"].as_array().unwrap().len() >= 10);
    assert!(initial["settings_error"].is_null());

    // Live edit settings.json externally and confirm hot-reload picks it up without restart
    fs::write(
        &settings_file,
        r#"{"lang":"en","backend_mode":"mock","defaults":{"poll_active_secs":20,"poll_idle_secs":180}}"#,
    )
    .unwrap();
    let p1 = rpc(
        &mut conn,
        &coord,
        &html,
        3,
        "tools/call",
        json!({ "name": "colab_app_poll", "arguments": {} }),
    );
    let after_edit = &p1["result"]["structuredContent"];
    assert_eq!(after_edit["settings"]["locale"], "en");
    assert_eq!(after_edit["settings"]["defaults"]["poll_active_secs"], 20);
    assert_eq!(after_edit["settings"]["defaults"]["poll_idle_secs"], 180);
    assert!(after_edit["settings_error"].is_null());

    // Invalid live edit fails closed: keeps last good settings, surfaces settings_error, never quarantines
    fs::write(&settings_file, "{bad json").unwrap();
    let p2 = rpc(
        &mut conn,
        &coord,
        &html,
        4,
        "tools/call",
        json!({ "name": "colab_app_poll", "arguments": {} }),
    );
    let after_bad = &p2["result"]["structuredContent"];
    assert_eq!(after_bad["settings"]["locale"], "en");
    assert!(after_bad["settings_error"].as_str().is_some());
    assert!(settings_file.exists());
    assert!(!settings_file.with_extension("json.corrupt").exists());

    // Tool error returns structured kind + localized EN/ZH messages
    let err_res = rpc(
        &mut conn,
        &coord,
        &html,
        5,
        "tools/call",
        json!({ "name": "colab_session_action", "arguments": { "action": "stop", "session": "nonexistent-session" } }),
    );
    assert_eq!(err_res["result"]["isError"], true);
    let body = &err_res["result"]["structuredContent"];
    assert_eq!(body["kind"], "not_found");
    assert!(!body["localized"]["en"].as_str().unwrap().is_empty());
    assert!(!body["localized"]["zh-CN"].as_str().unwrap().is_empty());

    // Verify HIGH_RAM casing normalization on ServerSession
    let sess = colab_mcp::cli::ServerSession {
        endpoint: "m-s-test".into(),
        name: Some("s1".into()),
        hardware: "G4".into(),
        accelerator: Some(colab_mcp::settings::Accelerator::G4),
        shape: "HIGH_RAM".into(),
        variant: "GPU".into(),
        status: Some("running".into()),
    };
    assert!(sess.high_mem());

    coord.shutdown();
}

#[test]
fn install_script_hardening_and_cli_fail_closed_without_home() {
    let install_sh = include_str!("../scripts/install.sh");
    assert!(install_sh.contains("--connect-timeout 2 --max-time 5"));
    assert!(install_sh.contains("-K -"));
    assert!(!install_sh.contains("-H \"x-codeium-csrf-token"));
    assert!(install_sh.contains("^(localhost|127\\.0\\.0\\.1|\\[::1\\]):[0-9]{1,5}$"));
    assert!(install_sh.contains("^v[0-9]+\\.[0-9]+\\.[0-9]+(-[0-9A-Za-z.-]+)?$"));
    assert!(install_sh.contains("^[0-9a-fA-F]{64}$"));

    // Runtime verification against newline-injected VERSION, LS_ADDR, and CSRF_TOKEN.
    let script_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install.sh");
    let bad_ver = std::process::Command::new("sh")
        .arg(&script_path)
        .arg("--version")
        .arg("v1.0.0\nevil")
        .arg("--no-reload")
        .output()
        .unwrap();
    assert!(!bad_ver.status.success());
    assert!(String::from_utf8_lossy(&bad_ver.stderr).contains("invalid release version tag"));

    let tmp = PrivateDir::new(&std::env::temp_dir(), "install-sh-test").unwrap();
    let fake_bin = tmp.path().join("bin");
    fs::create_dir(&fake_bin).unwrap();
    let marker = tmp.path().join("curl_called");
    let fake_curl = fake_bin.join("curl");
    fs::write(
        &fake_curl,
        format!(
            "#!/bin/sh\necho \"$*\" > \"{}\"\ncat >> \"{}\"\nprintf '200'\n",
            marker.display(),
            marker.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_curl, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let helper_end = install_sh.find("\ninstall_from_source()").unwrap();
    let fn_start = install_sh.find("is_single_line_regex()").unwrap();
    let harness = tmp.path().join("harness.sh");
    fs::write(
        &harness,
        format!(
            "#!/bin/sh\nset -eu\nNO_RELOAD=0\nFORCE_RELOAD=0\n{}\nrefresh_mcp_host\n",
            &install_sh[fn_start..helper_end]
        ),
    )
    .unwrap();
    let path_env = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    for (ls_addr, tok) in [
        ("127.0.0.1:5387\nurl = \"https://evil\"", "tok123"),
        ("127.0.0.1:99999", "tok123"),
        ("127.0.0.1:0", "tok123"),
        ("evil.example:5387", "tok123"),
        ("127.0.0.1:5387", "tok\nurl = \"https://evil\""),
        ("127.0.0.1:5387", "tok\"evil"),
    ] {
        let _ = fs::remove_file(&marker);
        let out = std::process::Command::new("sh")
            .arg(&harness)
            .env("PATH", &path_env)
            .env("ANTIGRAVITY_LS_ADDRESS", ls_addr)
            .env("ANTIGRAVITY_CSRF_TOKEN", tok)
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            !marker.exists(),
            "curl must not run for injected env: ls={ls_addr:?} tok={tok:?}"
        );
    }
    let ok = std::process::Command::new("sh")
        .arg(&harness)
        .env("PATH", &path_env)
        .env("ANTIGRAVITY_LS_ADDRESS", "[::1]:5387")
        .env("ANTIGRAVITY_CSRF_TOKEN", "csrf-valid-token")
        .output()
        .unwrap();
    assert!(ok.status.success());
    let recorded = fs::read_to_string(&marker).unwrap();
    assert!(recorded.contains("-K -"));
    assert!(recorded.contains(
        "http://[::1]:5387/exa.language_server_pb.LanguageServerService/RefreshMcpServers"
    ));
    assert!(!recorded.contains("-H"));
    assert!(recorded.contains("header = \"x-codeium-csrf-token: csrf-valid-token\""));

    let bin = env!("CARGO_BIN_EXE_colab-mcp");
    let out = std::process::Command::new(bin)
        .arg("status")
        .arg("--mock")
        .env_clear()
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot determine data directory"));
}
