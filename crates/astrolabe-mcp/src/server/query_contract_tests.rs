//! Cross-adapter regressions: use actual request deserialization and tools,
//! tiny temporary fixtures only (no stdio service, watcher or external LSP).
use super::*;

fn fixture(files: &[(&str, String)], ready: bool) -> (AstrolabeServer, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "astrolabe-query-contract-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    for (name, content) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let mut server = AstrolabeServer::new_with_cache_budget(dir.clone(), 8 << 20);
    server.structured_output = true;
    if ready {
        *server.primary.state.write().unwrap() =
            IndexState::Ready(Arc::new(crate::index::build(&dir).unwrap()));
    }
    (server, dir)
}
fn call(server: &AstrolabeServer, name: &str, args: Value) -> CallToolResult {
    server
        .dispatch_tool(name, args.as_object().cloned())
        .unwrap()
}
fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
fn meta(result: &CallToolResult) -> &Value {
    result.structured_content.as_ref().unwrap()
}

#[test]
fn query_contract_boolean_fields_accept_strings_without_truthiness() {
    for (raw, expected) in [
        (json!(true), true),
        (json!(false), false),
        (json!("true"), true),
        (json!("false"), false),
    ] {
        let p: SearchParams =
            serde_json::from_value(json!({"query":"needle", "regex":raw, "page":raw,
            "count_only":raw,"skip_comments":raw,"case_sensitive":raw,"group":raw,"dedup":raw}))
            .unwrap();
        assert_eq!(
            (p.regex, p.page, p.count_only, p.skip_comments),
            (expected, expected, expected, expected)
        );
        assert_eq!(
            (p.case_sensitive, p.group, p.dedup),
            (Some(expected), Some(expected), Some(expected))
        );
        let q: QueryParams = serde_json::from_value(
            json!({"query":"needle","include_body":raw,"substring":raw,"dedup":raw}),
        )
        .unwrap();
        assert_eq!(
            (q.include_body, q.substring, q.dedup),
            (expected, Some(expected), Some(expected))
        );
        let c: ResolveContextParams =
            serde_json::from_value(json!({"task_description":"needle","dedup":raw})).unwrap();
        assert_eq!(c.dedup, Some(expected));
        let d: crate::precise_tools::GetDiagParams =
            serde_json::from_value(json!({"path":"a.rs","lsp":raw})).unwrap();
        assert_eq!(d.lsp, Some(expected));
    }
    for raw in [None, Some(Value::Null)] {
        let mut args = json!({"query":"needle"});
        if let Some(raw) = raw {
            for key in ["case_sensitive", "group", "dedup"] {
                args[key] = raw.clone();
            }
        }
        let p: SearchParams = serde_json::from_value(args).unwrap();
        assert_eq!((p.case_sensitive, p.group, p.dedup), (None, None, None));
    }
    let q: QueryParams = serde_json::from_value(json!({"query":"needle"})).unwrap();
    assert!(q.include_body); // Preserve the non-false default.
    for raw in [
        json!("False"),
        json!("yes"),
        json!("1"),
        json!(1),
        json!([]),
        json!({}),
        Value::Null,
    ] {
        let error = serde_json::from_value::<SearchParams>(json!({"query":"needle","regex":raw}))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("JSON boolean") && error.contains("true") && error.contains("false"),
            "{error}"
        );
    }
    let schema = serde_json::to_value(schemars::schema_for!(SearchParams)).unwrap();
    assert_eq!(schema["properties"]["regex"]["type"], "boolean");
    let option = &schema["properties"]["group"];
    assert!(
        option.to_string().contains("boolean") && !option.to_string().contains("string"),
        "{option}"
    );
}

#[test]
fn query_contract_shared_dispatch_accepts_string_booleans() {
    let (server, dir) = fixture(&[("a.rs", "fn needle() {}\n".into())], true);
    let result = call(
        &server,
        "search_code",
        json!({"query":"needle","regex":"false","count_only":"true","skip_comments":"false"}),
    );
    assert_eq!(meta(&result)["total"], 1);
    let result = call(
        &server,
        "find_symbol",
        json!({"query":"needle","include_body":"false","substring":"false","dedup":"false"}),
    );
    assert_eq!(meta(&result)["total"], 1);
    let error = server
        .dispatch_tool(
            "search_code",
            json!({"query":"needle","group":"invalid"})
                .as_object()
                .cloned(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("JSON boolean"));
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn query_contract_line_only_reaches_graph_disambiguation() {
    let (server,dir)=fixture(&[("go.mod","module example.com/m\n".into()),("p/a.go",
        "package p\n\ntype A struct{}\ntype B struct{}\n\nfunc (a A) Name() string { return \"a\" }\n\nfunc (b B) Name() string { return \"b\" }\n\nfunc use() {\n\tx := A{}\n\ty := B{}\n\t_ = x.Name() + y.Name()\n}\n".into())],true);
    let result = call(
        &server,
        "find_refs",
        json!({"symbol":"Name","path":"p/a.go","line":7}),
    );
    let out = text(&result);
    assert_eq!(meta(&result)["matches"], 2, "{out}");
    assert!(!out.contains("grouped by definition"), "{out}");
    assert!(out.contains("p/a.go:8"), "{out}");
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn query_contract_recent_symbols_isolate_known_agents_and_ignore_unknown() {
    let (server, dir) = fixture(
        &[
            ("go.mod", "module example.com/m\n".into()),
            ("a/x.go", "package a\nfunc Init() {}\n".into()),
            ("b/y.go", "package b\nfunc Init() {}\n".into()),
        ],
        true,
    );
    for (agent, path) in [("agent-a", "a/"), ("agent-b", "b/")] {
        call(
            &server,
            "find_symbol",
            json!({"query":"Init","path_filter":path,"agent_id":agent,"include_body":false}),
        );
    }
    for (agent, path, other) in [
        ("agent-a", "a/x.go", "b/y.go"),
        ("agent-b", "b/y.go", "a/x.go"),
    ] {
        let result = dedup_agent::scope(agent.into(), || {
            call(&server, "find_refs", json!({"symbol":"Init"}))
        });
        let out = text(&result);
        assert!(
            out.contains("reusing this session's latest") && out.contains(path),
            "{out}"
        );
        // The ambiguity diagnostic may list other same-name definitions;
        // isolation concerns the inherited location and actual result rows.
        let reused = out
            .lines()
            .find(|line| line.contains("reusing this session's latest"))
            .unwrap();
        assert!(reused.contains(path) && !reused.contains(other), "{out}");
        assert!(out.contains(&format!("selected @{path}:")), "{out}");
        assert!(
            !out.lines()
                .any(|line| line.starts_with(&format!("{other} ("))),
            "{out}"
        );
    }
    let unknown = call(&server, "find_refs", json!({"symbol":"Init"}));
    assert!(!text(&unknown).contains("reusing this session's latest"));
    call(
        &server,
        "find_symbol",
        json!({"query":"Init","path_filter":"b/","include_body":false}),
    );
    let unknown = call(&server, "find_refs", json!({"symbol":"Init"}));
    assert!(!text(&unknown).contains("reusing this session's latest"));
    let explicit = dedup_agent::scope("agent-a".into(), || {
        call(
            &server,
            "find_refs",
            json!({"symbol":"Init","path":"b/y.go"}),
        )
    });
    assert!(!text(&explicit).contains("reusing this session's latest"));
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn query_contract_building_and_ready_share_filters_counts_and_validation() {
    let files = [
        (
            "src/a.rs",
            "// needle\nfn needle() {}\nconst S: &str = \"needle\";\n".into(),
        ),
        ("src/b.py", "def needle(): pass\n".into()),
        ("fixtures/data.json", "{\"needle\":true}\n".into()),
    ];
    for ready in [false, true] {
        let (server, dir) = fixture(&files, ready);
        let result = call(
            &server,
            "search_code",
            json!({"query":"needle","glob":"*.rs","skip_comments":"true","count_only":"true"}),
        );
        assert_eq!(
            meta(&result)["total"],
            1,
            "ready={ready}: {}",
            text(&result)
        );
        let result = call(
            &server,
            "search_code",
            json!({"query":"needle","path_filter":"src","glob":"*.rs","skip_comments":true,"aggregate":"file"}),
        );
        assert_eq!(
            meta(&result)["total"],
            1,
            "ready={ready}: {}",
            text(&result)
        );
        assert!(!text(&result).contains("fixtures/data.json"));
        for args in [
            json!({"query":""}),
            json!({"query":"a\\nb","regex":true}),
            json!({"query":"needle","aggregate":"invalid"}),
            json!({"query":"needle","glob":"["}),
        ] {
            let result = call(&server, "search_code", args);
            assert_eq!(
                result.is_error,
                Some(true),
                "ready={ready}: {}",
                text(&result)
            );
        }
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn query_contract_building_pages_obey_budget() {
    let source = (0..200)
        .map(|i| format!("fn needle_{i}() {{}}\n"))
        .collect::<String>();
    let (server, dir) = fixture(&[("a.rs", source)], false);
    let result = call(
        &server,
        "search_code",
        json!({"query":"needle","budget_tokens":500}),
    );
    let out = text(&result);
    assert_eq!(meta(&result)["total"], 200);
    let shown = meta(&result)["shown"].as_u64().unwrap();
    assert!(shown > 0 && shown < 200, "{out}");
    assert!(estimate_tokens(&out) <= 564, "{out}"); // finish_at adds root identity.
    let next = meta(&result)["next_offset"].as_u64().unwrap();
    assert_eq!(next, shown);
    let page = call(
        &server,
        "search_code",
        json!({"query":"needle","budget_tokens":500,"offset":next}),
    );
    assert_eq!(meta(&page)["offset"], next);
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn query_contract_requested_root_is_directory_not_substring() {
    let files = [
        ("src/a.rs", "fn needle() {}\n".into()),
        ("src-old/b.rs", "fn needle() {}\n".into()),
        ("other/src/c.rs", "fn needle() {}\n".into()),
    ];
    for ready in [false, true] {
        let (server, dir) = fixture(&files, ready);
        let requested = dir.join("src").to_string_lossy().into_owned();
        let result = call(
            &server,
            "search_code",
            json!({"query":"needle","index_root":requested,"glob":"*.rs","count_only":true}),
        );
        assert_eq!(meta(&result)["total"], 1, "{}", text(&result));
        if ready {
            let result = call(
                &server,
                "find_symbol",
                json!({"query":"needle","index_root":requested,"include_body":false}),
            );
            assert_eq!(meta(&result)["total"], 1, "{}", text(&result));
        }
        // A subsequent unscoped call must not inherit thread-local scope.
        let result = call(
            &server,
            "search_code",
            json!({"query":"needle","count_only":true}),
        );
        assert_eq!(meta(&result)["total"], 3, "{}", text(&result));
        drop(server);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn query_contract_requested_root_references_choose_in_scope_definition() {
    let (server, dir) = fixture(
        &[
            ("go.mod", "module example.com/m\n".into()),
            (
                "p/a.go",
                "package p\nfunc Init() {}\nfunc use(){ Init() }\n".into(),
            ),
            ("p-old/b.go", "package other\nfunc Init() {}\n".into()),
        ],
        true,
    );
    let result = call(
        &server,
        "find_refs",
        json!({"symbol":"Init","index_root":dir.join("p")}),
    );
    let out = text(&result);
    assert_eq!(meta(&result)["matches"], 2, "{out}");
    assert!(!out.contains("p-old/"), "{out}");
    assert!(!out.contains("no need to re-check with grep"), "{out}");
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn query_contract_default_name_query_keeps_duplicate_and_folded_semantics() {
    let (server, dir) = fixture(
        &[
            ("a.rs", "fn Foo() {}\n".into()),
            ("b.rs", "fn foo() {}\n".into()),
            ("c.rs", "fn FOO() {}\n".into()),
            ("d.rs", "fn FooBar() {}\n".into()),
        ],
        true,
    );
    let exact = call(
        &server,
        "find_symbol",
        json!({"query":"foo","include_body":false}),
    );
    assert_eq!(meta(&exact)["total"], 3, "{}", text(&exact));
    let all = call(
        &server,
        "find_symbol",
        json!({"query":"foo","include_body":false,"substring":true}),
    );
    assert_eq!(meta(&all)["total"], 4, "{}", text(&all));
    let out = text(&all);
    assert!(
        out.find("@b.rs:1").unwrap() < out.find("@a.rs:1").unwrap(),
        "{out}"
    );
    assert!(
        out.find("@c.rs:1").unwrap() < out.find("@d.rs:1").unwrap(),
        "{out}"
    );
    drop(server);
    std::fs::remove_dir_all(dir).unwrap();
}
