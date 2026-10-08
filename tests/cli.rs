use std::process::Command;

fn detangle(args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        // Configs see NODE_ENV/BABEL_ENV; keep the caller's shell out of it.
        .env_remove("NODE_ENV")
        .env_remove("BABEL_ENV")
        .output()
        .unwrap();
    (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
}

const FIXTURE: &str = "tests/fixtures/basic";

#[test]
fn check_reports_every_rule() {
    let (out, code) = detangle(&["check", FIXTURE, "-f", "json"]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let hits: Vec<(String, String, String)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["rule"].as_str().unwrap().into(),
                x["from"].as_str().unwrap().into(),
                x["to"].as_str().unwrap_or("").into(),
            )
        })
        .collect();
    let has = |rule: &str, from: &str, to: &str| {
        assert!(
            hits.iter().any(|h| h.0 == rule && h.1 == from && h.2 == to),
            "missing {rule}: {from} → {to} in {hits:#?}"
        )
    };
    has("no-undeclared-deps", "src/features/cart/price.ts", "lodash");
    has("not-to-dev-dep", "src/features/cart/price.ts", "vitest");
    has("not-to-test", "src/features/user/profile.ts", "src/utils/helper.test.ts");
    has("not-to-unresolvable", "src/features/user/profile.ts", "./does-not-exist");
    has("no-circular", "src/features/cart/cart.ts", "src/features/cart/price.ts");
    has("no-orphans", "src/utils/orphan.ts", "");
    // Type-only cycle user ↔ profile must not be reported; @types/estree is declared.
    assert!(!hits.iter().any(|h| h.0 == "no-circular" && h.1.contains("user/")));
    assert!(!hits.iter().any(|h| h.2 == "estree"));
    assert_eq!(hits.len(), 7);
}

#[test]
fn resolves_tsconfig_paths_and_js_extensions() {
    let (out, _) = detangle(&["graph", FIXTURE, "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let index = v["modules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "src/index.ts")
        .unwrap();
    let deps: Vec<&str> = index["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["module"].as_str().unwrap())
        .collect();
    assert!(deps.contains(&"src/features/cart/cart.ts"), "{deps:?}"); // "@/features/cart/cart"
    assert!(deps.contains(&"src/features/user/user.ts"), "{deps:?}"); // "./features/user/user.js"
    assert!(deps.contains(&"react") && deps.contains(&"fs"), "{deps:?}");
}

/// Unresolvable imports `check` reports, as (from, to).
fn unresolvable(fixture: &str) -> Vec<(String, String)> {
    let (out, _) = detangle(&["check", fixture, "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    v.as_array()
        .unwrap()
        .iter()
        .filter(|x| x["rule"] == "not-to-unresolvable")
        .map(|x| (x["from"].as_str().unwrap().into(), x["to"].as_str().unwrap().into()))
        .collect()
}

/// When the first matching `exports` condition points at a file that doesn't
/// exist, the next matching condition is tried, as TypeScript does (#18).
#[test]
fn exports_fall_through_to_the_next_condition() {
    let found = unresolvable("tests/fixtures/exports-fallthrough");
    assert!(!found.iter().any(|(from, _)| from == "src/a.ts"), "{found:?}");
}

/// With no condition's file on disk, the import is still unresolvable.
#[test]
fn exports_with_no_existing_target_stay_unresolvable() {
    let found = unresolvable("tests/fixtures/exports-fallthrough");
    assert!(found.contains(&("src/b.ts".into(), "sdk/missing.js".into())), "{found:?}");
}

#[test]
fn why_prints_shortest_chain() {
    let (out, code) = detangle(&["why", "src/index.ts", "helper.test.ts", "-C", FIXTURE]);
    assert_eq!(code, 0);
    let modules: Vec<&str> = out.lines().filter(|l| !l.starts_with(' ') && l.contains('/')).collect();
    assert_eq!(
        modules,
        [
            "src/index.ts",
            "src/features/cart/cart.ts",
            "src/features/user/profile.ts",
            "src/utils/helper.test.ts"
        ]
    );
}

#[test]
fn affected_lists_transitive_dependents() {
    let (out, _) = detangle(&["affected", "-C", FIXTURE, "src/features/user/user.ts"]);
    let got: Vec<&str> = out.lines().collect();
    assert_eq!(
        got,
        [
            "src/features/cart/cart.ts",
            "src/features/cart/price.ts",
            "src/features/user/profile.ts",
            "src/features/user/user.ts",
            "src/index.ts"
        ]
    );
}

#[test]
fn affected_why_explains_shortest_import_paths_and_changed_roots() {
    let (out, code) = detangle(&["affected", "-C", FIXTURE, "--why", "src/features/user/user.ts"]);
    assert_eq!(code, 0);
    assert_eq!(out, concat!(
        "src/features/cart/cart.ts → src/features/user/profile.ts → src/features/user/user.ts (changed)\n",
        "src/features/cart/price.ts → src/features/cart/cart.ts → src/features/user/profile.ts → src/features/user/user.ts (changed)\n",
        "src/features/user/profile.ts → src/features/user/user.ts (changed)\n",
        "src/features/user/user.ts (changed)\n",
        "src/index.ts → src/features/user/user.ts (changed)\n",
    ));
}

fn affected_project(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("detangle-affected-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"affected-fixture","private":true}"#).unwrap();
    for (path, contents) in files {
        std::fs::write(dir.join(path), contents).unwrap();
    }
    dir
}

fn affected_run(dir: &std::path::Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
        .arg("affected").args(args).current_dir(dir).env("NO_COLOR", "1")
        .env_remove("NODE_ENV").env_remove("BABEL_ENV").output().unwrap();
    (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap(), out.status.code().unwrap())
}

#[test]
fn affected_json_filters_after_resource_traversal_and_retains_merged_edge_evidence() {
    let dir = affected_project("json-resources", &[
        ("src/data.json", "{}"),
        ("src/site.css", "body {}"),
        ("src/app.ts", "import './data.json'; import './site.css';"),
        ("src/app.test.ts", "import './app'; void import('./app.ts');"),
    ]);
    for seed in ["src/data.json", "src/site.css"] {
        let (out, err, code) = affected_run(&dir, &["-f", "json", seed, "--filter", r"\.test\.ts$"]);
        assert_eq!(code, 0, "{err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["counts"], serde_json::json!({"inputs":1,"seeds":1,"affected":3,"displayed":1}));
        assert_eq!(report["scope"]["outputFilter"], r"\.test\.ts$");
        assert_eq!(report["affected"][0]["path"], serde_json::json!(["src/app.test.ts","src/app.ts",seed]));
        assert_eq!(report["affected"][0]["edges"][0], serde_json::json!({"from":"src/app.test.ts","to":"src/app.ts","specifiers":["./app","./app.ts"],"dependencyTypes":["local"]}));
    }
    let (out, err, code) = affected_run(&dir, &["--format", "text", "src/site.css"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "src/app.test.ts\nsrc/app.ts\nsrc/site.css\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_json_reports_empty_results_restrictions_and_analysis_gaps() {
    let dir = affected_project("json-limits", &[
        ("src/a.ts", "import './missing';"),
        ("src/b.ts", "export const b = ;"),
        ("detangle.toml", "[options]\nexclude_path = '^excluded/'\ninclude_only = '^(src/|[.]/)'\ndo_not_follow = 'opaque'\n"),
    ]);
    for args in [vec!["-f", "json"], vec!["-f", "json", "package.json", "unknown.ts"]] {
        let (out, err, code) = affected_run(&dir, &args);
        assert_eq!(code, 0, "{err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["affected"], serde_json::json!([]));
        assert_eq!(report["counts"], serde_json::json!({"inputs":args.len()-2,"seeds":0,"affected":0,"displayed":0}));
        assert_eq!(report["scope"]["graphRestrictions"], serde_json::json!({"exclude_path":"^excluded/","include_only":"^(src/|[.]/)","do_not_follow":"opaque"}));
        let limits = report["limitations"].as_array().unwrap();
        for code in ["graph-restrictions", "unresolved-imports", "parse-errors", "dynamic-relationships-not-guaranteed"] {
            assert!(limits.iter().any(|l| l["code"] == code), "{report}");
        }
        let unresolved = limits.iter().find(|l| l["code"] == "unresolved-imports").unwrap();
        assert_eq!(unresolved["paths"], serde_json::json!(["src/a.ts"]));
        assert_eq!(unresolved["count"], 1);
        let errors = limits.iter().find(|l| l["code"] == "parse-errors").unwrap();
        assert_eq!(errors["paths"], serde_json::json!(["src/b.ts"]));
        assert!(errors["count"].as_u64().unwrap() > 0);
        if args.len() == 2 { assert_eq!(report["inputs"], serde_json::json!([])); }
        else {
            assert_eq!(report["inputs"][0]["classification"], "configuration");
            assert_eq!(report["inputs"][1]["classification"], "unmatched");
        }
    }
    for args in [vec!["-f", "json", "--filter", "["], vec!["-f", "json", "--since", "absent"]] {
        let (out, err, code) = affected_run(&dir, &args);
        assert_ne!(code, 0, "{err}");
        assert!(out.is_empty(), "{out}");
    }
    std::fs::write(dir.join("detangle.toml"), "[invalid").unwrap();
    let (out, err, code) = affected_run(&dir, &["-f", "json"]);
    assert_ne!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_json_preserves_git_comparison_origins_and_deleted_configuration() {
    let dir = affected_project("json-git", &[
        ("src/root.ts", "export const root = 1;"),
        ("src/gone.ts", "export const gone = 1;"),
        ("src/app.ts", "import './root';"),
        ("tsconfig.json", "{}"),
    ]);
    let git = |args: &[&str]| {
        let out = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args).current_dir(&dir).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]); git(&["add", "-A"]); git(&["commit", "-qm", "base"]);
    let base = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-qb", "feature"]);
    std::fs::write(dir.join("src/root.ts"), "export const root = 2;").unwrap();
    std::fs::remove_file(dir.join("src/gone.ts")).unwrap();
    std::fs::remove_file(dir.join("tsconfig.json")).unwrap();
    git(&["commit", "-qam", "feature"]);
    let head = git(&["rev-parse", "HEAD"]);
    git(&["checkout", "-q", "main"]);
    std::fs::write(dir.join("src/app.ts"), "import './root'; // main only").unwrap();
    git(&["commit", "-qam", "main moves"]); git(&["checkout", "-q", "feature"]);
    std::fs::write(dir.join("src/new.ts"), "export const n = 1;").unwrap();
    let (out, err, code) = affected_run(&dir, &["--format", "json", "--since", "main", "src/root.ts", "unknown.ts"]);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["comparison"], serde_json::json!({"mode":"merge-base-to-worktree","requestedReference":"main","resolvedBase":base,"head":head}));
    assert_eq!(report["counts"], serde_json::json!({"inputs":5,"seeds":2,"affected":3,"displayed":3}));
    assert_eq!(report["inputs"], serde_json::json!([
        {"path":"src/gone.ts","origins":{"explicit":false,"git":true},"classification":"deleted","module":null,"deleted":true},
        {"path":"src/new.ts","origins":{"explicit":false,"git":true},"classification":"module","module":"src/new.ts","deleted":false},
        {"path":"src/root.ts","origins":{"explicit":true,"git":true},"classification":"module","module":"src/root.ts","deleted":false},
        {"path":"tsconfig.json","origins":{"explicit":false,"git":true},"classification":"configuration","module":null,"deleted":true},
        {"path":"unknown.ts","origins":{"explicit":true,"git":false},"classification":"unmatched","module":null,"deleted":false}
    ]));
    let limitations = report["limitations"].as_array().unwrap();
    assert_eq!(limitations.iter().map(|l| l["code"].as_str().unwrap()).collect::<Vec<_>>(), ["historical-edges-not-analyzed","deleted-inputs-not-followed","configuration-impact-not-expanded","unmatched-inputs","dynamic-relationships-not-guaranteed"]);
    assert_eq!(limitations[1]["paths"], serde_json::json!(["src/gone.ts","tsconfig.json"]));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_json_reports_explicit_evidence_and_unique_seeds() {
    let dir = affected_project("json-explicit", &[
        ("src/root.ts", "export type Root = string;"),
        ("src/app.ts", "import type { Root } from './root'; export type App = Root;"),
        ("src/app.test.ts", "import './app';"),
    ]);
    let args = ["-f", "json", "src/root.ts", "./src/root.ts", "src/root.ts"];
    let (out, err, code) = affected_run(&dir, &args);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["comparison"], serde_json::json!({"mode":"explicit", "requestedReference":null,"resolvedBase":null,"head":null}));
    assert_eq!(report["scope"], serde_json::json!({"root":std::fs::canonicalize(&dir).unwrap(),"basis":"current-graph","graphRestrictions":{},"outputFilter":null}));
    assert_eq!(report["counts"], serde_json::json!({"inputs":2,"seeds":1,"affected":3,"displayed":3}));
    assert_eq!(report["inputs"], serde_json::json!([
        {"path":"./src/root.ts","origins":{"explicit":true,"git":false},"classification":"module","module":"src/root.ts","deleted":false},
        {"path":"src/root.ts","origins":{"explicit":true,"git":false},"classification":"module","module":"src/root.ts","deleted":false}
    ]));
    assert_eq!(report["affected"][0], serde_json::json!({"module":"src/app.test.ts","reason":"dependent","seed":"src/root.ts",
        "path":["src/app.test.ts","src/app.ts","src/root.ts"],"edges":[
            {"from":"src/app.test.ts","to":"src/app.ts","specifiers":["./app"],"dependencyTypes":["local"]},
            {"from":"src/app.ts","to":"src/root.ts","specifiers":["./root"],"dependencyTypes":["local","type-only"]}
        ]}));
    assert_eq!(report["affected"][2], serde_json::json!({"module":"src/root.ts","reason":"changed","seed":"src/root.ts","path":["src/root.ts"],"edges":[]}));
    assert_eq!(report["limitations"][0]["code"], "dynamic-relationships-not-guaranteed");
    let (with_why, err, code) = affected_run(&dir, &["--format", "json", "--why", "src/root.ts", "./src/root.ts"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&with_why).unwrap(), report);
    let (out, err, code) = affected_run(&dir, &["-f", "json", "src/root.ts", "--filter", "no-match"]);
    assert_eq!(code, 0, "{err}");
    let filtered: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(filtered["affected"], serde_json::json!([]));
    assert_eq!(filtered["counts"], serde_json::json!({"inputs":1,"seeds":1,"affected":3,"displayed":0}));
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn affected_why_quotes_unusual_paths_without_splitting_explanations() {
    let dir = affected_project("escaped", &[
        ("src/a test.ts", "import './line\\nfeed';"),
        ("src/line\nfeed.ts", "export const n = 1;"),
    ]);
    let (out, err, code) = affected_run(&dir, &["--why", "src/line\nfeed.ts"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "\"src/a test.ts\" → \"src/line\\nfeed.ts\" (changed)\n\"src/line\\nfeed.ts\" (changed)\n");
    let (out, err, code) = affected_run(&dir, &["-f", "json", "src/line\nfeed.ts"]);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["affected"][0]["path"], serde_json::json!(["src/a test.ts", "src/line\nfeed.ts"]));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_why_reports_current_graph_scope_and_available_limitations() {
    let dir = affected_project("limits", &[
        ("src/a.ts", "import './missing';"),
        ("src/b.ts", "export const n = ;"),
        ("detangle.toml", "[options]\nexclude_path = '^excluded/'\ninclude_only = '^(src/|[.]/)'\ndo_not_follow = 'opaque'\n"),
    ]);
    let (out, err, code) = affected_run(&dir, &["--why", "package.json", "unknown.ts", "--filter", "test"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    for expected in ["current-graph static dependency reachability", "filter=\"test\"", "exclude_path", "include_only", "do_not_follow",
        "configuration-impact-not-expanded", "package.json", "unmatched-inputs", "unknown.ts", "graph-restrictions",
        "unresolved-imports", "parse-errors", "src/a.ts", "dynamic-relationships-not-guaranteed"] {
        assert!(err.contains(expected), "missing {expected}: {err}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(unix)]
#[test]
fn affected_why_rejects_unsupported_git_path_encoding() {
    use std::io::Write;
    let dir = affected_project("encoding", &[("src/a.ts", "export const a = 1;")]);
    for args in [vec!["init", "-q"], vec!["add", "-A"], vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "base"]] {
        assert!(Command::new("git").args(args).current_dir(&dir).output().unwrap().status.success());
    }
    // Put the raw pathname in Git's index: some filesystems cannot create it.
    let blob = Command::new("git").args(["rev-parse", "HEAD:src/a.ts"]).current_dir(&dir).output().unwrap();
    let mut entry = format!("100644 {}\t", String::from_utf8(blob.stdout).unwrap().trim()).into_bytes();
    entry.extend_from_slice(b"bad-\xff.txt\0");
    let mut index = Command::new("git").args(["update-index", "-z", "--index-info"]).current_dir(&dir)
        .stdin(std::process::Stdio::piped()).spawn().unwrap();
    index.stdin.take().unwrap().write_all(&entry).unwrap();
    assert!(index.wait().unwrap().success());
    assert!(Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "raw path"])
        .current_dir(&dir).output().unwrap().status.success());
    let (out, err, code) = affected_run(&dir, &["--why", "--since", "HEAD"]);
    assert_ne!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("unsupported Git path encoding") && err.contains("UTF-8"), "{err}");
    let (out, err, code) = affected_run(&dir, &["--format", "json", "--since", "HEAD"]);
    assert_ne!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("unsupported Git path encoding"), "{err}");
    let (_, err, code) = affected_run(&dir, &["--since", "HEAD"]);
    assert_eq!(code, 0, "{err}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_why_chooses_stable_shortest_paths_with_diamonds_cycles_and_multiple_roots() {
    let dir = affected_project("ties", &[
        ("src/root-a.ts", "import './root-z'; export const a = 1;"),
        ("src/root-z.ts", "export const z = 1;"),
        ("src/a.ts", "import './root-a'; import './b';"),
        ("src/b.ts", "import './root-a'; import './a';"),
        ("src/diamond.ts", "import './b'; import './a';"),
        ("src/both.ts", "import './root-z'; import './root-a';"),
    ]);
    let expected = concat!(
        "src/a.ts → src/root-a.ts (changed)\n",
        "src/b.ts → src/root-a.ts (changed)\n",
        "src/both.ts → src/root-a.ts (changed)\n",
        "src/diamond.ts → src/a.ts → src/root-a.ts (changed)\n",
        "src/root-a.ts (changed)\nsrc/root-z.ts (changed)\n",
    );
    for roots in [["src/root-z.ts", "src/root-a.ts", "./src/root-a.ts"], ["src/root-a.ts", "src/root-a.ts", "src/root-z.ts"]] {
        let (out, err, code) = affected_run(&dir, &["--why", roots[0], roots[1], roots[2]]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, expected);
        assert!(err.contains("2 changed modules → 6 affected"), "{err}");
        let (out, err, code) = affected_run(&dir, &["-f", "json", roots[0], roots[1], roots[2]]);
        assert_eq!(code, 0, "{err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        let records = report["affected"].as_array().unwrap();
        assert_eq!(records.iter().map(|r| &r["path"]).collect::<Vec<_>>(), vec![
            &serde_json::json!(["src/a.ts", "src/root-a.ts"]),
            &serde_json::json!(["src/b.ts", "src/root-a.ts"]),
            &serde_json::json!(["src/both.ts", "src/root-a.ts"]),
            &serde_json::json!(["src/diamond.ts", "src/a.ts", "src/root-a.ts"]),
            &serde_json::json!(["src/root-a.ts"]),
            &serde_json::json!(["src/root-z.ts"]),
        ]);
        assert_eq!(report["counts"]["seeds"], 2);
        assert_eq!(records[4]["seed"], "src/root-a.ts");
        assert_eq!(records[5]["seed"], "src/root-z.ts");
        assert_eq!(records[5]["edges"], serde_json::json!([]));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_why_filters_results_after_following_resources_and_type_only_edges() {
    let dir = affected_project("resources", &[
        ("src/style.css", "body {}"),
        ("src/data.json", "{}"),
        ("src/types.ts", "export type A = string;"),
        ("src/app.ts", "import './style.css'; import data from './data.json'; import type { A } from './types'; export type B = A;"),
        ("src/app.test.ts", "import type { B } from './app';"),
    ]);
    for root in ["src/style.css", "src/data.json", "src/types.ts"] {
        let (out, err, code) = affected_run(&dir, &["--why", root, "--filter", r"\.test\.ts$"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, format!("src/app.test.ts → src/app.ts → {root} (changed)\n"));
        let (out, err, code) = affected_run(&dir, &[root]);
        assert_eq!(code, 0, "{err}");
        let mut expected = vec!["src/app.test.ts", "src/app.ts", root];
        expected.sort();
        assert_eq!(out.lines().collect::<Vec<_>>(), expected);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_why_respects_restrictions_without_inventing_edges() {
    let dir = affected_project("restricted", &[
        ("src/root.ts", "export const a = 1;"),
        ("src/middle.ts", "import './root';"),
        ("src/test.ts", "import './middle';"),
    ]);
    for setting in ["exclude_path = 'middle'", "include_only = '(root|test)'", "do_not_follow = 'middle'"] {
        std::fs::write(dir.join("detangle.toml"), format!("[options]\n{setting}\n")).unwrap();
        let (out, err, code) = affected_run(&dir, &["--why", "src/root.ts"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, "src/root.ts (changed)\n", "{setting}: {err}");
        assert!(err.contains("graph-restrictions"), "{err}");
    }
    // A retained do-not-follow node can itself be a seed through incoming edges.
    let (out, err, code) = affected_run(&dir, &["--why", "src/middle.ts"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "src/middle.ts (changed)\nsrc/test.ts → src/middle.ts (changed)\n");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_reports_type_only_restrictions_without_changing_selection() {
    let dir = affected_project("type-only-restriction", &[
        ("src/types.ts", "export type Value = string;"),
        ("src/app.ts", "import type { Value } from './types'; export const value: Value = 'a';"),
    ]);
    for enabled in [false, true] {
        std::fs::write(dir.join("detangle.toml"), format!("[options]\nignore_type_only = {enabled}\n")).unwrap();
        let expected = if enabled { "src/types.ts\n" } else { "src/app.ts\nsrc/types.ts\n" };
        let (plain, err, code) = affected_run(&dir, &["src/types.ts"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(plain, expected);
        let (out, err, code) = affected_run(&dir, &["--why", "src/types.ts"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, if enabled { "src/types.ts (changed)\n" } else { "src/app.ts → src/types.ts (changed)\nsrc/types.ts (changed)\n" });
        assert_eq!(err.contains("graph restriction: ignore_type_only=true"), enabled, "{err}");
        assert_eq!(err.contains("limitation [graph-restrictions]"), enabled, "{err}");
        let (out, err, code) = affected_run(&dir, &["-f", "json", "src/types.ts"]);
        assert_eq!(code, 0, "{err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["scope"]["graphRestrictions"], if enabled { serde_json::json!({"ignore_type_only":true}) } else { serde_json::json!({}) });
        assert_eq!(report["affected"].as_array().unwrap().iter().map(|m| m["module"].as_str().unwrap()).collect::<Vec<_>>(), expected.lines().collect::<Vec<_>>());
        assert_eq!(report["limitations"].as_array().unwrap().iter().any(|l| l["code"] == "graph-restrictions"), enabled);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_reports_dynamic_import_restrictions_without_changing_selection() {
    let dir = affected_project("dynamic-restriction", &[
        ("src/lazy.ts", "export const value = 'a';"),
        ("src/app.ts", "export const load = () => import('./lazy');"),
    ]);
    for enabled in [false, true] {
        std::fs::write(dir.join("detangle.toml"), format!("[options]\nexclude_dynamic = {enabled}\n")).unwrap();
        let expected = if enabled { "src/lazy.ts\n" } else { "src/app.ts\nsrc/lazy.ts\n" };
        let (plain, err, code) = affected_run(&dir, &["src/lazy.ts"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(plain, expected);
        let (out, err, code) = affected_run(&dir, &["--why", "src/lazy.ts"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(out, if enabled { "src/lazy.ts (changed)\n" } else { "src/app.ts → src/lazy.ts (changed)\nsrc/lazy.ts (changed)\n" });
        assert_eq!(err.contains("graph restriction: exclude_dynamic=true"), enabled, "{err}");
        assert_eq!(err.contains("limitation [graph-restrictions]"), enabled, "{err}");
        let (out, err, code) = affected_run(&dir, &["-f", "json", "src/lazy.ts"]);
        assert_eq!(code, 0, "{err}");
        let report: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(report["scope"]["graphRestrictions"], if enabled { serde_json::json!({"exclude_dynamic":true}) } else { serde_json::json!({}) });
        assert_eq!(report["affected"].as_array().unwrap().iter().map(|m| m["module"].as_str().unwrap()).collect::<Vec<_>>(), expected.lines().collect::<Vec<_>>());
        assert_eq!(report["limitations"].as_array().unwrap().iter().any(|l| l["code"] == "graph-restrictions"), enabled);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_why_empty_results_and_fatal_inputs_are_distinct() {
    let dir = affected_project("failures", &[("src/a.ts", "export const a = 1;")]);
    for args in [vec!["--why"], vec!["--why", "src/a.ts", "--filter", "no-matches"], vec!["--why", "absent.ts"]] {
        let (out, err, code) = affected_run(&dir, &args);
        assert_eq!(code, 0, "{err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("current-graph"), "{err}");
    }
    for args in [vec!["--why", "src/a.ts", "--filter", "["], vec!["--why", "--since", "HEAD"], vec!["--why", "-C", "nonexistent-directory"]] {
        let (out, err, code) = affected_run(&dir, &args);
        assert_ne!(code, 0, "{err}");
        assert!(out.is_empty(), "{out}");
        assert!(err.contains("error:"), "{err}");
    }
    std::fs::write(dir.join("detangle.toml"), "[broken config").unwrap();
    let (out, err, code) = affected_run(&dir, &["--why", "src/a.ts"]);
    assert_ne!(code, 0, "{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("error:"), "{err}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_does_not_report_empty_patterns_or_disabled_graph_restrictions() {
    let dir = affected_project("empty-restrictions", &[
        ("src/a.ts", "export const a = 1;"),
        ("detangle.toml", "[options]\nexclude_path = ''\ninclude_only = ''\ndo_not_follow = ''\nignore_type_only = false\nexclude_dynamic = false\n"),
    ]);
    let (out, err, code) = affected_run(&dir, &["--why", "src/a.ts"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "src/a.ts (changed)\n");
    assert!(!err.contains("graph-restrictions") && !err.contains("graph restriction:"), "{err}");
    let (out, err, code) = affected_run(&dir, &["-f", "json", "src/a.ts"]);
    assert_eq!(code, 0, "{err}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["scope"]["graphRestrictions"], serde_json::json!({}));
    assert!(!report["limitations"].as_array().unwrap().iter().any(|l| l["code"] == "graph-restrictions"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn affected_since_compares_from_the_merge_base() {
    let dir = std::env::temp_dir().join(format!("detangle-affected-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let write = |p: &str, s: &str| std::fs::write(dir.join(p), s).unwrap();
    write("package.json", r#"{"name":"r","private":true}"#);
    write("src/a.ts", "import { b } from './b';\nexport const a = b;\n");
    write("src/b.ts", "export const b = 1;\n");
    write("src/c.ts", "export const c = 1;\n");
    write("src/d.ts", "import { c } from './c';\nexport const d = c;\n");
    write("src/gone.ts", "export const gone = 1;\n");
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "base"]);
    git(&["checkout", "-qb", "feature"]);
    // The branch: b.ts edited and gone.ts deleted (committed), package.json
    // edited (unstaged), new.ts added (untracked).
    write("src/b.ts", "export const b = 2;\n");
    std::fs::remove_file(dir.join("src/gone.ts")).unwrap();
    git(&["commit", "-qam", "feature"]);
    write("package.json", r#"{"name":"r","private":true,"version":"1.0.0"}"#);
    write("src/new.ts", "export const n = 1;\n"); // imports nothing that changed
    // main moves on after the branch: c.ts changes there.
    git(&["checkout", "-q", "main"]);
    write("src/c.ts", "export const c = 2;\n");
    git(&["commit", "-qm", "main moves on", "src/c.ts"]);
    git(&["checkout", "-q", "feature"]);

    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap(), out.status.code().unwrap())
    };
    let (out, err, code) = run(&["affected", "--since", "main"]);
    assert_eq!(code, 0, "{err}");
    // c.ts changed only on main, so neither it nor d.ts is affected.
    assert_eq!(out.lines().collect::<Vec<_>>(), ["src/a.ts", "src/b.ts", "src/new.ts"], "{err}");
    assert!(err.contains("2 changed modules, 1 deleted, 1 config → 3 affected"), "{err}");
    assert!(err.contains("deleted: src/gone.ts"), "{err}");
    assert!(err.contains("config changed: package.json"), "{err}");

    let (out, err, code) = run(&["affected", "--since", "main", "--why"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "src/a.ts → src/b.ts (changed)\nsrc/b.ts (changed)\nsrc/new.ts (changed)\n");
    for expected in ["historical-edges-not-analyzed", "deleted-inputs-not-followed", "configuration-impact-not-expanded", "src/gone.ts", "package.json"] {
        assert!(err.contains(expected), "{err}");
    }
    let (_, err, code) = run(&["affected", "--why", "--since", "no-such-ref"]);
    assert_ne!(code, 0);
    assert!(err.contains("no-such-ref"), "{err}");

    let (_, err, code) = run(&["affected", "--since", "no-such-ref"]);
    assert_ne!(code, 0);
    assert!(err.contains("no-such-ref"), "{err}");
    write("tsconfig.json", "{}");
    git(&["add", "tsconfig.json"]);
    git(&["commit", "-qm", "tracked config"]);
    std::fs::remove_file(dir.join("tsconfig.json")).unwrap();
    let (_, err, code) = run(&["affected", "--why", "--since", "HEAD"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.lines().any(|line| line.contains("deleted-inputs-not-followed") && line.contains("tsconfig.json")), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn backreference_rules() {
    let dir = std::env::temp_dir().join(format!("detangle-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("detangle.toml");
    std::fs::write(
        &cfg,
        r#"
[[forbidden]]
name = "no-cross-feature"
severity = "error"
from = { path = '^src/features/([^/]+)/' }
to = { path = '^src/features/', path_not = '^src/features/$1/' }
"#,
    )
    .unwrap();
    let (out, code) = detangle(&["check", FIXTURE, "-c", cfg.to_str().unwrap(), "-f", "json"]);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let pairs: Vec<(&str, &str)> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["from"].as_str().unwrap(), x["to"].as_str().unwrap()))
        .collect();
    assert_eq!(pairs, [("src/features/cart/cart.ts", "src/features/user/profile.ts")]);
}

#[test]
fn baseline_suppresses_known_violations() {
    let bl = std::env::temp_dir().join(format!("detangle-bl-{}.json", std::process::id()));
    let bl = bl.to_str().unwrap();
    assert_eq!(detangle(&["check", FIXTURE, "--write-baseline", bl]).1, 0);
    let (_, code) = detangle(&["check", FIXTURE, "--baseline", bl]);
    std::fs::remove_file(bl).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn vue_svelte_angular() {
    let (out, _) = detangle(&["graph", "tests/fixtures/frameworks", "-f", "json", "--externals"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let mut edges: Vec<String> = vec![];
    for m in v["modules"].as_array().unwrap() {
        for d in m["dependencies"].as_array().unwrap() {
            let types: Vec<&str> = d["types"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
            edges.push(format!("{} -> {} [{}]", m["id"].as_str().unwrap(), d["module"].as_str().unwrap(), types.join(",")));
        }
    }
    for want in [
        "src/main.ts -> src/App.vue [local]",
        "src/main.ts -> src/components/Button.svelte [local]",
        "src/App.vue -> src/components/Hello.vue [local]",
        "src/App.vue -> src/types.ts [local,type-only]",
        "src/components/Hello.vue -> src/util.ts [local]",
        "src/components/Button.svelte -> src/util.ts [local]",
        "src/components/Button.svelte -> svelte [npm]",
        "src/app/app.component.ts -> @angular/core [npm]",
        "src/app/app.component.ts -> src/app/app.component.html [local,resource]",
        "src/app/app.component.ts -> ./missing.component.css [unresolvable,resource]",
        "src/app/app.component.ts -> src/util.ts [local,dynamic]",
    ] {
        assert!(edges.iter().any(|e| e == want), "missing {want}\n{edges:#?}");
    }
    // `<script>` inside the template and the CDN script in <svelte:head> are not imports.
    assert!(!edges.iter().any(|e| e.contains("cdn.example") || e.contains("not code")), "{edges:#?}");
}

#[test]
fn runs_javascript_rule_configs() {
    let (out, code) = detangle(&["check", FIXTURE, "-c", "tests/fixtures/rules.config.json", "-f", "json"]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let mut got: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| format!("{} {} -> {}", x["rule"].as_str().unwrap(), x["from"].as_str().unwrap(), x["to"].as_str().unwrap_or("-")))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "no-circular src/features/cart/cart.ts -> src/features/cart/price.ts",
            "no-circular src/features/cart/price.ts -> src/features/cart/cart.ts",
            "no-cross-feature src/features/cart/cart.ts -> src/features/user/profile.ts",
            // `node_modules/react/` also matches the npm package.
            "no-non-package-json src/features/cart/price.ts -> lodash",
            "not-to-unresolvable src/features/user/profile.ts -> ./does-not-exist",
            // helper.test.ts has one dependent; orphan.ts has none.
            "utils-must-be-shared src/utils/helper.test.ts -> -",
            "utils-must-be-shared src/utils/orphan.ts -> -",
        ]
    );
}

#[test]
fn init_converts_javascript_rule_configs() {
    let dir = std::env::temp_dir().join(format!("detangle-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
        .args(["init", dir.to_str().unwrap(), "--from", "tests/fixtures/rules.config.json"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let toml = std::fs::read_to_string(dir.join("detangle.toml")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("6 forbidden, 0 allowed, 1 required"), "{stdout}");
    assert!(toml.contains("\"npm-bundled\""), "{toml}");
    assert!(toml.contains("path_not = \"^src/features/$1/\""), "{toml}");
}

#[test]
fn html_report_embeds_the_analysis() {
    let file = std::env::temp_dir().join(format!("detangle-report-{}.html", std::process::id()));
    let (out, code) = detangle(&["report", FIXTURE, "-o", file.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let html = std::fs::read_to_string(&file).unwrap();
    std::fs::remove_file(&file).unwrap();
    let start = html.find(r#"<script id="data" type="application/json">"#).unwrap();
    let body = &html[start..];
    let json = &body[body.find('>').unwrap() + 1..body.find("</script>").unwrap()];
    // Nothing inside the data block may close the script tag early.
    assert!(!json.contains("</"));
    let d: serde_json::Value = serde_json::from_str(json).unwrap();
    assert_eq!(d["project"], "basic");
    assert_eq!(d["v"].as_array().unwrap().len(), 7);
    assert_eq!(d["e"].as_array().unwrap().len() % 3, 0);
    assert!(d["m"].as_array().unwrap().iter().any(|m| m == "src/features/cart/cart.ts"));
    // Self-contained: no external resources.
    assert!(!html.contains("http://") && !html.contains("https://"));
}

#[test]
fn webpack_babel_and_native_aliases() {
    let (out, _) = detangle(&["graph", "tests/fixtures/aliases", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let index = v["modules"].as_array().unwrap().iter().find(|m| m["id"] == "src/index.js").unwrap();
    let mut got: Vec<String> = index["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| format!("{} -> {}", d["specifier"].as_str().unwrap(), d["module"].as_str().unwrap()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            "@components/Button -> src/components/Button.jsx", // webpack alias
            "@feature/cart -> src/features/cart/index.js",     // babel regex alias
            "@lib/strings -> src/lib/strings.js",              // detangle.toml alias
            "rootmod -> src/roots/rootmod.js",                 // babel root
            "theme -> src/shared/theme.js",                    // webpack resolve.modules
            "utils -> src/utils/index.js",                     // webpack `utils$`
            "utils/other -> utils/other",                      // `$` is exact: unresolved
            "~/components/Card -> src/components/Card.jsx",    // babel alias
            // "legacy-lib" is aliased to `false`: no dependency at all.
        ]
    );
}

#[test]
fn vite_aliases() {
    let (out, _) = detangle(&["graph", "tests/fixtures/vite", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let mut got: Vec<String> = vec![];
    for m in v["modules"].as_array().unwrap().iter().filter(|m| m["id"].as_str().unwrap().starts_with("src/")) {
        for d in m["dependencies"].as_array().unwrap() {
            got.push(format!("{}: {} -> {}", m["id"].as_str().unwrap(), d["specifier"].as_str().unwrap(), d["module"].as_str().unwrap()));
        }
    }
    got.sort();
    assert_eq!(
        got,
        [
            "src/App.vue: @/components/Card.vue -> src/components/Card.vue", // inside <script setup>
            "src/main.ts: #utils/format -> src/utils/format.ts",             // root-relative "/src/utils"
            "src/main.ts: @/App.vue -> src/App.vue",                         // fileURLToPath(new URL(...))
            "src/main.ts: rel/thing -> src/local/thing.ts",                  // relative: from the importer
            "src/main.ts: ~/theme -> src/shared/theme.ts",                   // RegExp find with $1
        ]
    );
}

/// What each alias in tests/fixtures/config-env resolved to, e.g. "mode/production".
fn config_env_targets(extra: &[&str]) -> Vec<String> {
    let mut args = vec!["graph", "tests/fixtures/config-env", "-f", "json"];
    args.extend(extra);
    let (out, code) = detangle(&args);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let index = v["modules"].as_array().unwrap().iter().find(|m| m["id"] == "src/index.js").unwrap();
    index["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            let m = d["module"].as_str().unwrap();
            m.trim_start_matches("src/").trim_end_matches("/index.js").to_string()
        })
        .collect()
}

#[test]
fn config_env_defaults() {
    assert_eq!(
        config_env_targets(&[]),
        ["api/dev", "mode/development", "node-env/development", "var/none", "cli/serve", "cmd/serve", "vmode/development", "benv/development", "dotenv/base", "expanded/base-x"]
    );
}

#[test]
fn config_env_from_detangle_toml() {
    let cfg = std::env::temp_dir().join(format!("detangle-config-env-{}.toml", std::process::id()));
    std::fs::write(
        &cfg,
        r#"
[options]
webpack_config = "webpack.config.js"
vite_config = "vite.config.mjs"
babel_config = "babel.config.js"

[options.config_env]
mode = "production"
command = "build"
webpack_env = { production = true }
vars = { API_TARGET = "staging" }
"#,
    )
    .unwrap();
    let got = config_env_targets(&["-c", cfg.to_str().unwrap()]);
    std::fs::remove_file(&cfg).unwrap();
    assert_eq!(
        got,
        // NODE_ENV follows mode/command; Babel's api.env() follows NODE_ENV.
        ["api/prod", "mode/production", "node-env/production", "var/staging", "cli/build", "cmd/build", "vmode/production", "benv/production", "dotenv/prod", "expanded/prod-x"]
    );
}

#[test]
fn config_env_mode_flag() {
    let got = config_env_targets(&["--mode", "staging"]);
    assert_eq!(got[1], "mode/staging");
    assert_eq!(got[6], "vmode/staging");
}

/// Like `config_env_targets`, with extra environment variables for detangle.
fn config_env_targets_with(extra: &[&str], vars: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec!["graph", "tests/fixtures/config-env", "-f", "json"];
    args.extend(extra);
    let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
        .args(&args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("NODE_ENV")
        .env_remove("BABEL_ENV")
        .envs(vars.iter().copied())
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let index = v["modules"].as_array().unwrap().iter().find(|m| m["id"] == "src/index.js").unwrap();
    index["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["module"].as_str().unwrap().trim_start_matches("src/").trim_end_matches("/index.js").to_string())
        .filter(|m| m.starts_with("dotenv/") || m.starts_with("expanded/"))
        .collect()
}

#[test]
fn dotenv_precedence() {
    // Shell environment beats .env files (and feeds their expansions).
    assert_eq!(config_env_targets_with(&[], &[("DOTENV_TARGET", "shell")]), ["dotenv/shell", "expanded/shell-x"]);
    // detangle.toml `vars` beat the shell.
    let cfg = std::env::temp_dir().join(format!("detangle-dotenv-vars-{}.toml", std::process::id()));
    std::fs::write(
        &cfg,
        "[options]\nwebpack_config = \"webpack.config.js\"\n[options.config_env]\nvars = { DOTENV_TARGET = \"vars\" }\n",
    )
    .unwrap();
    let with_vars = config_env_targets_with(&["-c", cfg.to_str().unwrap()], &[("DOTENV_TARGET", "shell")]);
    // `env_files = false` turns loading off.
    std::fs::write(&cfg, "[options]\nwebpack_config = \"webpack.config.js\"\n[options.config_env]\nenv_files = false\n").unwrap();
    let disabled = config_env_targets_with(&["-c", cfg.to_str().unwrap()], &[]);
    std::fs::remove_file(&cfg).unwrap();
    // Expansion inside .env uses the same precedence, so `vars` win there too.
    assert_eq!(with_vars, ["dotenv/vars", "expanded/vars-x"]);
    assert_eq!(disabled, ["dotenv/none", "expanded/none"]);
}

#[test]
fn migrate_dry_run_merges_every_source() {
    let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
        .args(["migrate", "tests/fixtures/migrate", "--dry-run"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let toml_text = String::from_utf8(out.stdout).unwrap();
    let summary = String::from_utf8(out.stderr).unwrap();
    // Nothing is written in a dry run.
    assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migrate/detangle.toml").exists());

    let cfg: toml::Value = toml::from_str(&toml_text).unwrap();
    let names: Vec<&str> = cfg["forbidden"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["no-circular", "app-not-to-test", "import/no-cycle", "import/no-restricted-paths"]);
    // madge's and the rules config's circular rules merged at the stricter severity.
    assert_eq!(cfg["forbidden"][0]["severity"].as_str(), Some("error"));
    // ESLint: the test-file override turned into path_not; the zone kept its message.
    assert_eq!(cfg["forbidden"][2]["from"]["path_not"].as_str(), Some(r"^(?:.*/)?[^/]*\.test\.ts$"));
    assert_eq!(cfg["forbidden"][3]["comment"].as_str(), Some("lib must not depend on app"));
    // maxDepth 3: cycles of up to 4 modules.
    assert_eq!(cfg["forbidden"][2]["to"]["max_cycle_length"].as_integer(), Some(4));
    let options = &cfg["options"];
    assert_eq!(options["exclude_path"].as_str(), Some(r"\.stories\.ts$")); // madge
    assert!(options["exclude"].as_array().unwrap().iter().any(|g| g.as_str() == Some("generated/**"))); // ESLint ignorePatterns
    assert_eq!(options["baseline"].as_str(), Some(".detangle-baseline.json"));

    for expected in [
        "known.json  1 known violations",
        "\"deps\": \"some-dep-checker --config .deps-rules.json src\"",
        "\"cycles\": \"madge --circular --extensions ts src\"",
        "\"cycles\": \"detangle check\"",
    ] {
        assert!(summary.contains(expected), "missing {expected:?} in:\n{summary}");
    }
}

#[test]
fn migrate_writes_config_and_baseline() {
    let dir = std::env::temp_dir().join(format!("detangle-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migrate"), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
    };
    let (out, code) = run(&["migrate"]);
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("detangle.toml").is_file() && dir.join(".detangle-baseline.json").is_file());
    // A second run refuses to overwrite.
    assert_ne!(run(&["migrate"]).1, 0);
    // The known violation is baselined; the lib ⇄ app cycle is new.
    let (out, _) = run(&["check", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let rules: Vec<&str> = v.as_array().unwrap().iter().map(|x| x["rule"].as_str().unwrap()).collect();
    assert!(!rules.contains(&"app-not-to-test"), "{rules:?}");
    assert!(rules.contains(&"no-circular") && rules.contains(&"import/no-restricted-paths"), "{rules:?}");
    // Accept today's findings; check is then clean.
    assert_eq!(run(&["check", "--write-baseline"]).1, 0);
    assert_eq!(run(&["check"]).1, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn empty_exclude_keeps_the_project() {
    // The rules config the JavaScript rules tool generates has `exclude: []`.
    // Migrated, it must not exclude every file; nor may an empty pattern
    // written in detangle.toml.
    let dir = std::env::temp_dir().join(format!("detangle-empty-exclude-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"r","private":true}"#).unwrap();
    std::fs::write(dir.join("src/a.ts"), "import { b } from './b';\nexport const a = 1;\n").unwrap();
    std::fs::write(dir.join("src/b.ts"), "import { a } from './a';\nexport const b = a;\n").unwrap();
    std::fs::write(
        dir.join(".rules.cjs"),
        "module.exports = { forbidden: [{ name: 'no-circular', severity: 'error', from: {}, to: { circular: true } }], options: { exclude: [], doNotFollow: [] } };\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
    };
    let cycles = |out: &str| {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v.as_array().unwrap().iter().filter(|x| x["rule"] == "no-circular").count()
    };
    assert_eq!(run(&["migrate"]).1, 0);
    let (out, code) = run(&["check", "-f", "json"]);
    assert_eq!((cycles(&out), code), (2, 1), "{out}");

    std::fs::write(
        dir.join("detangle.toml"),
        "[options]\nexclude_path = \"\"\ndo_not_follow = \"\"\n\n[[forbidden]]\nname = \"no-circular\"\nseverity = \"error\"\nto = { circular = true }\n",
    )
    .unwrap();
    let (out, _) = run(&["check", "-f", "json"]);
    assert_eq!(cycles(&out), 2, "{out}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn says_when_declared_packages_are_not_installed() {
    // A fresh CI checkout without `npm ci`: every import of a declared
    // package is unresolvable. `check` says why, once.
    let dir = std::env::temp_dir().join(format!("detangle-not-installed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"r","private":true,"dependencies":{"dayjs":"1","@s/ui":"1"}}"#).unwrap();
    std::fs::write(dir.join("src/a.ts"), "import d from 'dayjs';\nimport { b } from '@s/ui/button';\nimport x from 'undeclared';\nexport const a = [d, b, x];\n").unwrap();
    std::fs::write(dir.join("src/b.ts"), "import d from 'dayjs';\nexport const c = d;\n").unwrap();
    let run = |format: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(["check", "-f", format]).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };
    let note = "3 imports of 2 declared packages don't resolve because they aren't installed (@s/ui, dayjs)";
    let (_, err) = run("text");
    assert!(err.contains(note), "{err}");
    let (out, _) = run("github");
    assert!(out.lines().any(|l| l.starts_with("::warning title=detangle::") && l.contains(note)), "{out}");
    let (out, _) = run("markdown");
    assert!(out.contains(note), "{out}");
    let (out, err) = run("json");
    assert!(err.contains(note) && serde_json::from_str::<serde_json::Value>(&out).is_ok(), "{err}");

    // Installed, though not completely: no note. `@s/ui` and `undeclared`
    // are ordinary unresolvable imports then.
    std::fs::create_dir_all(dir.join("node_modules/dayjs")).unwrap();
    let (_, err) = run("text");
    assert!(!err.contains("installed"), "{err}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn tsconfig_with_a_missing_extends_keeps_its_own_paths() {
    // `extends` names a package that isn't installed: TypeScript reports it
    // and still applies the file's own `paths` and `baseUrl`. So does detangle.
    let extending = |options: &str| {
        format!("{{\n  // a comment, as tsconfigs have\n  \"extends\": \"some-missing-pkg/tsconfig.base\",\n  \"compilerOptions\": {options},\n}}\n")
    };
    for (name, files, spec) in [
        ("paths", vec![("tsconfig.json", extending(r#"{"paths": {"@src/*": ["./src/*"]}}"#))], "@src/"),
        ("base-url", vec![("tsconfig.json", extending(r#"{"baseUrl": "src"}"#))], ""),
        // A solution-style tsconfig: its own `paths`, and `references` to a
        // project whose `extends` chain reaches the missing package.
        (
            "references",
            vec![
                ("tsconfig.json", r#"{"compilerOptions": {"paths": {"@src/*": ["./src/*"]}}, "files": [], "references": [{"path": "./tsconfig.app.json"}]}"#.to_string()),
                ("tsconfig.app.json", r#"{"extends": "./tsconfig.base.json", "include": ["src"]}"#.to_string()),
                ("tsconfig.base.json", extending(r#"{"paths": {"@src/*": ["./src/*"]}}"#)),
            ],
            "@src/",
        ),
    ] {
        let dir = std::env::temp_dir().join(format!("detangle-ts-extends-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("package.json"), r#"{"name":"r","private":true}"#).unwrap();
        for (file, body) in &files {
            std::fs::write(dir.join(file), body).unwrap();
        }
        std::fs::write(dir.join("src/a.ts"), format!("import {{ b }} from '{spec}b';\nexport const a = 1;\n")).unwrap();
        std::fs::write(dir.join("src/b.ts"), format!("import {{ a }} from '{spec}a';\nexport const b = a;\n")).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(["check", "-f", "json"]).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        let (stdout, stderr) = (String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap());
        let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let rules: Vec<&str> = v.as_array().unwrap().iter().map(|x| x["rule"].as_str().unwrap()).collect();
        assert_eq!(rules, ["no-circular", "no-circular"], "{name}: {stdout}");
        assert!(stderr.contains("tsconfig.json") && stderr.contains("some-missing-pkg/tsconfig.base"), "{name}: {stderr}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

/// Migrates a fixture copy and returns the imports flagged by `detangle check`
/// as (file, specifier) — group-scope violations expanded to their imports.
fn migrated_flagged_imports(fixture: &str) -> std::collections::BTreeSet<(String, String)> {
    let dir = std::env::temp_dir().join(format!("detangle-{fixture}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    run(&["migrate"]);
    let violations: serde_json::Value = serde_json::from_str(&run(&["check", "-f", "json"])).unwrap();
    let graph: serde_json::Value = serde_json::from_str(&run(&["graph", "-f", "json", "--externals"])).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let mut out = std::collections::BTreeSet::new();
    for v in violations.as_array().unwrap() {
        if v["scope"] == "group" {
            // Group violations list the imports behind them.
            for i in v["imports"].as_array().unwrap() {
                out.insert((i["from"].as_str().unwrap().to_string(), i["specifier"].as_str().unwrap().to_string()));
            }
            continue;
        }
        let (from, to) = (v["from"].as_str().unwrap(), v["to"].as_str());
        for m in graph["modules"].as_array().unwrap() {
            if m["id"] != from {
                continue;
            }
            for d in m["dependencies"].as_array().unwrap() {
                if to == d["module"].as_str() {
                    out.insert((from.to_string(), d["specifier"].as_str().unwrap().to_string()));
                }
            }
        }
    }
    out
}

#[test]
fn nx_boundaries_match_nx() {
    // Exactly the imports real Nx 21 (@nx/enforce-module-boundaries) flags on this workspace.
    let expected: std::collections::BTreeSet<(String, String)> = [
        ("apps/shop/src/index.js", "@org/shop-feature"),
        ("libs/admin/feature/src/index.js", "../../../shared/util/src/index.js"),
        ("libs/legacy/src/index.js", "@org/shared-util"),
        ("libs/shared/data/src/index.js", "@org/shared-util"),
        ("libs/shared/util/src/index.js", "@org/shared-data"),
        ("libs/shop/feature/src/index.js", "@org/admin-feature"),
        ("libs/shop/feature/src/index.js", "@org/shop-ui"),
        ("libs/shop/feature/src/index.js", "@org/shop-app"),
        ("libs/shop/ui/src/index.js", "@org/shop-feature"),
        ("libs/shop/ui/src/index.js", "lodash"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(migrated_flagged_imports("nx-workspace"), expected);
}

#[test]
fn element_boundaries_match_eslint_plugin_boundaries() {
    // Exactly the imports real eslint-plugin-boundaries 7.2 flags on this project.
    let expected: std::collections::BTreeSet<(String, String)> = [
        ("src/features/cart/index.js", "../user/index.js"),
        ("src/features/cart/index.js", "../../utils/fmt.js"),
        ("src/ui/button/index.js", "../../features/cart/index.js"),
        ("src/utils/fmt.js", "../app/main.js"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(migrated_flagged_imports("boundaries"), expected);
}

/// (file, specifier) pairs from a list of `("file", "specifier")`.
fn pairs(list: &[(&str, &str)]) -> std::collections::BTreeSet<(String, String)> {
    list.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

#[test]
fn no_cycle_max_depth_matches_eslint_plugin_import() {
    // Exactly the imports eslint-plugin-import 2.32 no-cycle flags with maxDepth 2:
    // cycles of up to 3 modules (a↔b, c→d→e, j↔k, j→l→m), not f→g→h→i.
    let expected = pairs(&[
        ("src/a.js", "./b.js"),
        ("src/b.js", "./a.js"),
        ("src/c.js", "./d.js"),
        ("src/d.js", "./e.js"),
        ("src/e.js", "./c.js"),
        ("src/j.js", "./k.js"),
        ("src/j.js", "./l.js"),
        ("src/k.js", "./j.js"),
        ("src/l.js", "./m.js"),
        ("src/m.js", "./j.js"),
    ]);
    assert_eq!(migrated_flagged_imports("cycle-depth"), expected);
}

#[test]
fn eslint_extends_match_eslint() {
    // Exactly what ESLint 9 (legacy config mode) reports: no-restricted-paths
    // from a relative extends, no-cycle's maxDepth 1 from a shareable config
    // kept by the root's severity-only "warn", and src/core re-enabled by it.
    let expected = pairs(&[
        ("src/app/a.js", "./b.js"),
        ("src/app/a.js", "../lib/l.js"),
        ("src/app/b.js", "./a.js"),
        ("src/core/x.js", "./y.js"),
        ("src/core/y.js", "./x.js"),
        ("src/lib/l.js", "../app/a.js"),
    ]);
    assert_eq!(migrated_flagged_imports("eslint-extends"), expected);
}

#[test]
fn nx_options_match_nx() {
    // Exactly the imports real Nx 21 flags with allow, enforceBuildableLibDependency,
    // banTransitiveDependencies and checkNestedExternalImports on, including the
    // transitive notDependOnLibsWithTags, an empty onlyDependOnLibsWithTags, a
    // workspaces package without an `nx` section, projectType inferred from
    // tsconfig.app.json, a self-import through the project's alias, a relative
    // import outside every project and a static import of a lazy-loaded lib.
    // `allow` exempts @org/legacy; require() isn't checked.
    let expected = pairs(&[
        ("apps/app/src/static.js", "@org/ui"),
        ("libs/bridge/src/index.js", "@org/server"),
        ("libs/bridge/src/index.js", "chalk"),
        ("libs/empty/src/index.js", "@org/feat"),
        ("libs/feat/src/index.js", "@org/bridge"),
        ("libs/feat/src/index.js", "@org/ui"),
        ("libs/feat/src/index.js", "left-pad"),
        ("libs/ui/src/index.js", "not-installed-pkg"),
        ("libs/ui/src/more.js", "@org/app-e2e"),
        ("libs/ui/src/more.js", "@org/notype"),
        ("libs/ui/src/more.js", "@org/plain"),
        ("libs/ui/src/self.js", "@org/ui"),
        ("libs/util/src/index.js", "../../../tools/helper.js"),
    ]);
    assert_eq!(migrated_flagged_imports("nx-options"), expected);
}

#[test]
fn boundaries_captures_match_eslint_plugin_boundaries() {
    // Exactly what real eslint-plugin-boundaries 7.2 reports: captured values
    // compared through `{{ from.element.captured.x }}`, `{{ from.x }}` and legacy
    // `${from.x}` templates, a literal captured value in a later disallow,
    // components nested in modules (the innermost element wins), no policy
    // matching component → module (no `default`, so disallowed), ignored test
    // files, entry-point, external (a banned subpath too) and no-unknown.
    let expected = pairs(&[
        ("src/app/main.js", "../misc/stuff.js"),
        ("src/helpers/format/index.js", "../internal/index.js"),
        ("src/helpers/format/index.js", "fs"),
        ("src/helpers/format/index.js", "lodash/fp"),
        ("src/modules/auth/components/login/index.js", "../../../cart/components/list/index.js"),
        ("src/modules/auth/components/login/index.js", "../../index.js"),
        ("src/modules/auth/components/login/index.js", "lodash"),
        ("src/modules/auth/index.js", "../../helpers/format/util.js"),
        ("src/modules/auth/index.js", "../../helpers/internal/index.js"),
        ("src/modules/auth/index.js", "../cart/components/list/index.js"),
    ]);
    assert_eq!(migrated_flagged_imports("boundaries-captures"), expected);
}

#[test]
fn boundaries_legacy_syntax_matches_eslint_plugin_boundaries() {
    // Exactly what real eslint-plugin-boundaries 7.2 reports for the legacy
    // `element-types` format: `["type", { captured }]` selectors on both sides
    // (`${from.domain}` and a literal source condition), basePattern +
    // baseCapture, boundaries/include (scripts/ is left out) and
    // dependency-nodes ["import"] (require(), import() and re-exports unchecked).
    let expected = pairs(&[
        ("src/domains/shop/features/list/index.js", "../../../billing/features/pay/index.js"),
        ("src/shared/util/index.js", "../../domains/shop/features/cart/index.js"),
    ]);
    assert_eq!(migrated_flagged_imports("boundaries-legacy"), expected);
}

#[test]
fn rule_conditions_match_the_js_rules_tool() {
    // What the JavaScript rules tool flags per dependency with these rules:
    // ancestor (true/false), exoticallyRequired / exoticRequire(Not),
    // npm-bundled, and viaOnly / via with dependency types (a type-only
    // cycle is left out, the cycle through import() is caught).
    let (out, _) = detangle(&["check", "tests/fixtures/conditions", "-c", "tests/fixtures/conditions/rules.config.cjs", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let got: std::collections::BTreeSet<String> =
        v.as_array().unwrap().iter().map(|x| format!("{} {} -> {}", x["rule"].as_str().unwrap(), x["from"].as_str().unwrap(), x["to"].as_str().unwrap())).collect();
    let want: std::collections::BTreeSet<String> = [
        "bundled src/a/b/deep.ts -> lodash",
        "exotic src/a/b/deep.ts -> src/a/b/ex1.ts",
        "exotic src/a/b/deep.ts -> src/a/b/ex2.ts",
        "lazy-cycles src/c6.ts -> src/c7.ts",
        "lazy-cycles src/c7.ts -> src/c6.ts",
        "need src/a/b/deep.ts -> src/a/b/ex2.ts",
        "not-need src/a/b/deep.ts -> src/a/b/ex1.ts",
        "not-up src/a/b/deep.ts -> left",
        "not-up src/a/b/deep.ts -> lodash",
        "not-up src/a/b/deep.ts -> src/a/b/ex1.ts",
        "not-up src/a/b/deep.ts -> src/a/b/ex2.ts",
        "not-up src/a/b/deep.ts -> src/a/b/sib.ts",
        "up src/a/b/deep.ts -> src/a/mid.ts",
        "up src/a/b/deep.ts -> src/top.ts",
        "value-cycles src/c3.ts -> src/c4.ts",
        "value-cycles src/c4.ts -> src/c5.ts",
        "value-cycles src/c5.ts -> src/c3.ts",
        "value-cycles src/c6.ts -> src/c7.ts",
        "value-cycles src/c7.ts -> src/c6.ts",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(got, want);
}

#[test]
fn ci_formats_and_baseline_maintenance() {
    let dir = std::env::temp_dir().join(format!("detangle-ci-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nx-options"), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
    };
    run(&["migrate"]);
    let (md, code) = run(&["check", "-f", "markdown"]);
    assert_eq!(code, 1);
    assert!(md.contains("❌ **13 errors, 0 warnings, 0 info**"), "{md}");
    assert!(md.contains("| `@nx/enforce-module-boundaries` | error | 13 |"), "{md}");
    assert!(md.contains("  - via `libs/ui/src/more.js` importing `@org/plain`"), "{md}");
    let (tc, _) = run(&["check", "-f", "teamcity"]);
    assert_eq!(tc.lines().filter(|l| l.starts_with("##teamcity[inspectionType ")).count(), 1);
    assert!(tc.contains("file='apps/app/src/static.js' SEVERITY='ERROR'"), "{tc}");
    let (az, _) = run(&["check", "-f", "azure"]);
    assert!(az.contains("##vso[task.logissue type=error;sourcepath=libs/ui/src/self.js;code=@nx/enforce-module-boundaries;]"), "{az}");
    assert!(az.ends_with("##vso[task.complete result=Failed;]13 errors, 0 warnings, 0 info\n"), "{az}");

    // Baseline: fix some violations, add a new one.
    run(&["check", "--write-baseline", "b.json"]);
    std::fs::write(dir.join("libs/feat/src/index.js"), "export const feat = 1;\n").unwrap();
    std::fs::write(dir.join("libs/util/src/new.js"), "import { ui } from \"@org/ui\";\nexport const n = ui;\n").unwrap();
    let cfg = std::fs::read_to_string(dir.join("detangle.toml")).unwrap().replace("[options]", "[options]\nbaseline_stale = \"warn\"");
    std::fs::write(dir.join("detangle.toml"), cfg).unwrap();
    let (json, _) = run(&["check", "--baseline", "b.json", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let stale = v.as_array().unwrap().iter().filter(|x| x["rule"] == "stale-baseline-entry").count();
    assert_eq!(stale, 3, "{json}");
    run(&["check", "--write-baseline", "b.json", "--baseline-mode", "shrink-only"]);
    let kept: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(dir.join("b.json")).unwrap()).unwrap();
    // Fixed entries dropped; the new violation isn't added.
    assert_eq!(kept.len(), 10);
    let (text, code) = run(&["check", "--baseline", "b.json"]);
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(code, 1);
    assert!(text.contains("libs/util/src/new.js") && !text.contains("stale-baseline-entry"), "{text}");
}

#[test]
fn graph_filters_select_like_the_js_rules_tool() {
    // Module sets the JavaScript rules tool selects on this fixture.
    let ids = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["graph", "tests/fixtures/conditions", "-c", "tests/fixtures/empty.toml", "-f", "json"];
        all.extend(args);
        let (out, _) = detangle(&all);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let mut ids: Vec<String> =
            v["modules"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).filter(|id| id.starts_with("src/")).map(String::from).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&["--focus", "src/c4"]), ["src/c3.ts", "src/c4.ts", "src/c5.ts"]);
    assert_eq!(ids(&["--focus", "mid"]), ["src/a/b/deep.ts", "src/a/mid.ts"]);
    assert_eq!(ids(&["--reaches", "top"]), ["src/a/b/deep.ts", "src/top.ts"]);
    assert_eq!(ids(&["--focus", "c6", "--reaches", "c7"]), ["src/c6.ts", "src/c7.ts"]);
    let (json, _) = detangle(&["graph", "tests/fixtures/conditions", "-c", "tests/fixtures/empty.toml", "-f", "json", "--highlight", "c[12]"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let lit: Vec<&str> = v["modules"].as_array().unwrap().iter().filter(|m| m["highlighted"] == true).filter_map(|m| m["id"].as_str()).collect();
    assert_eq!(lit, ["src/c1.ts", "src/c2.ts"]);
    let (mmd, _) = detangle(&["graph", "tests/fixtures/conditions", "-c", "tests/fixtures/empty.toml", "-f", "mermaid", "--collapse", "^src/[^/]+/", "--highlight", "deep"]);
    // src/a/ collapses a/mid.ts and a/b/*; it's highlighted because deep.ts is in it.
    assert!(mmd.contains("[\"src/a/\"]:::highlight"), "{mmd}");
    assert!(!mmd.contains("src/a/mid.ts"), "{mmd}");
}

#[test]
fn graph_csv_matrix_and_d2() {
    let (csv, _) = detangle(&["graph", "tests/fixtures/conditions", "-c", "tests/fixtures/empty.toml", "-f", "csv", "--focus", "c6"]);
    // Same matrix layout as the JavaScript rules tool: header, "true"/"false" cells, trailing empty column.
    assert_eq!(
        csv,
        "\"\",\"src/c6.ts\",\"src/c7.ts\",\"\"\n\"src/c6.ts\",\"false\",\"true\",\"\"\n\"src/c7.ts\",\"true\",\"false\",\"\"\n"
    );
    let (d2, _) = detangle(&["graph", "tests/fixtures/conditions", "-c", "tests/fixtures/empty.toml", "-f", "d2", "--focus", "c6"]);
    assert!(d2.contains("\"src\".\"c6.ts\": {class: cycle; link: \"src/c6.ts\"}"), "{d2}");
    assert!(d2.contains("\"src\".\"c6.ts\" -> \"src\".\"c7.ts\": {style.stroke: \"#dd3333\"; style.stroke-dash: 3}"), "{d2}");
}

#[test]
fn do_not_follow_exclude_dynamic_and_max_depth() {
    // Same results as the JavaScript rules tool on the cycle-depth fixture
    // (c → d → e → c, j → k/l, …).
    let dir = std::env::temp_dir().join(format!("detangle-follow-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cycle-depth"), &dir);
    std::fs::remove_file(dir.join(".eslintrc.json")).unwrap();
    std::fs::write(dir.join("src/lazy.js"), "export const lazy = () => import(\"./a.js\");\n").unwrap();
    std::fs::write(dir.join("detangle.toml"), "[options]\ndo_not_follow = 'src/d\\.js'\nexclude_dynamic = true\n").unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let deps = |v: &serde_json::Value, id: &str| -> Vec<String> {
        v["modules"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap()["dependencies"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["module"].as_str().unwrap().to_string())
            .collect()
    };
    let all = run(&["graph", "-f", "json"]);
    assert!(deps(&all, "src/d.js").is_empty()); // not followed, but still a module
    assert_eq!(deps(&all, "src/c.js"), ["src/d.js"]);
    assert!(deps(&all, "src/lazy.js").is_empty()); // dynamic import excluded
    let ids = |v: serde_json::Value| -> Vec<String> {
        let mut ids: Vec<String> = v["modules"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(run(&["graph", "-f", "json", "--from", "^src/j", "--max-depth", "1"])), ["src/j.js", "src/k.js", "src/l.js"]);
    assert_eq!(ids(run(&["graph", "-f", "json", "--from", "^src/j", "--max-depth", "2"])), ["src/j.js", "src/k.js", "src/l.js", "src/m.js"]);
    // From c: d isn't followed, so e is never reached.
    assert_eq!(ids(run(&["graph", "-f", "json", "--from", "^src/c"])), ["src/c.js", "src/d.js"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn parse_cache_reuses_and_notices_edits() {
    for strategy in ["metadata", "content"] {
        let dir = std::env::temp_dir().join(format!("detangle-cache-{strategy}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cycle-depth"), &dir);
        std::fs::remove_file(dir.join(".eslintrc.json")).unwrap();
        let deps_of_a = || {
            let out = Command::new(env!("CARGO_BIN_EXE_detangle"))
                .args(["graph", "-f", "json", "--cache", "--cache-strategy", strategy])
                .current_dir(&dir)
                .output()
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            let a = v["modules"].as_array().unwrap().iter().find(|m| m["id"] == "src/a.js").unwrap().clone();
            a["dependencies"].as_array().unwrap().iter().map(|d| d["module"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(deps_of_a(), ["src/b.js"]);
        assert!(dir.join("node_modules/.cache/detangle/parse-cache.bin").is_file());
        assert_eq!(deps_of_a(), ["src/b.js"]); // from the cache
        // Same size, new content: the edit must still be seen.
        std::fs::write(dir.join("src/a.js"), "import { c } from \"./c.js\";\nexport const a = () => [c];\n").unwrap();
        assert_eq!(deps_of_a(), ["src/c.js"], "{strategy}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[test]
fn node_api() {
    // The npm package's own tests, against this build.
    let Ok(out) = Command::new("node")
        .args(["--test", "npm/test.mjs"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("DETANGLE_BIN", env!("CARGO_BIN_EXE_detangle"))
        .env_remove("NODE_ENV")
        .output()
    else {
        eprintln!("node not installed; skipping");
        return;
    };
    assert!(out.status.success(), "{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// The pre-commit hook installs detangle from npm; it must install the
/// version being released (docs/releasing.md).
#[test]
fn pre_commit_hook_installs_this_version() {
    let hooks = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.pre-commit-hooks.yaml")).unwrap();
    let want = format!("\"detangle@{}\"", env!("CARGO_PKG_VERSION"));
    assert!(hooks.contains(&want), "{want} not in .pre-commit-hooks.yaml");
}

/// `check -f github` annotations as (file, line, rule).
fn github_annotations(fixture: &str) -> Vec<(String, Option<u32>, String)> {
    let (out, _) = detangle(&["check", fixture, "-f", "github"]);
    out.lines()
        .filter_map(|l| l.strip_prefix("::"))
        .map(|l| {
            let props = l.split_once(' ').unwrap().1.split("::").next().unwrap();
            let get = |k: &str| props.split(',').find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from));
            (get("file").unwrap(), get("line").map(|n| n.parse().unwrap()), get("title").unwrap())
        })
        .collect()
}

/// A GitHub annotation lands on the line of the import that causes it.
#[test]
fn github_annotations_point_at_the_import_line() {
    let found = github_annotations("tests/fixtures/github-lines");
    assert!(found.contains(&("src/a.ts".into(), Some(3), "not-to-unresolvable".into())), "{found:?}");
}

/// Each import on a cycle is annotated at its own line, whatever its quotes.
#[test]
fn github_cycle_annotations_point_at_each_import() {
    let found = github_annotations("tests/fixtures/github-lines");
    assert!(found.contains(&("src/a.ts".into(), Some(2), "no-circular".into())), "{found:?}");
    assert!(found.contains(&("src/b.ts".into(), Some(3), "no-circular".into())), "{found:?}");
}

/// The same string in a comment above the import doesn't take the annotation.
#[test]
fn github_annotations_skip_comments() {
    let found = github_annotations("tests/fixtures/github-lines");
    assert!(found.contains(&("src/c.ts".into(), Some(3), "not-to-unresolvable".into())), "{found:?}");
}

/// A package whose `exports` map the `custom` condition to sources (build
/// output absent), a tsconfig declaring it through `extends`, and the
/// unresolvable imports `check` reports with `toml` as detangle.toml.
fn custom_condition_unresolvable(name: &str, toml: Option<&str>) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("detangle-custom-conditions-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
    std::fs::write(dir.join("package.json"), r#"{"name":"r","private":true}"#).unwrap();
    std::fs::write(dir.join("tsconfig.base.json"), r#"{"compilerOptions": {"customConditions": ["custom"]}}"#).unwrap();
    std::fs::write(dir.join("tsconfig.json"), r#"{"extends": "./tsconfig.base.json"}"#).unwrap();
    std::fs::write(
        dir.join("node_modules/pkg/package.json"),
        r#"{"name":"pkg","exports":{"./*":{"custom":"./*.ts","default":"./out/*.js"}}}"#,
    )
    .unwrap();
    std::fs::write(dir.join("node_modules/pkg/util.ts"), "export const u = 1;\n").unwrap();
    std::fs::write(dir.join("src/a.ts"), "import { u } from 'pkg/util';\nexport const a = u;\n").unwrap();
    if let Some(t) = toml {
        std::fs::write(dir.join("detangle.toml"), t).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(["check", "-f", "json"]).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    v.as_array().unwrap().iter().filter(|x| x["rule"] == "not-to-unresolvable").map(|x| x["to"].as_str().unwrap().to_string()).collect()
}

#[test]
fn tsconfig_custom_conditions_resolve_exports() {
    let found = custom_condition_unresolvable("on", None);
    assert!(found.is_empty(), "{found:?}");
}

#[test]
fn explicit_condition_names_win_over_tsconfig_custom_conditions() {
    let toml = "[options.resolve]\ncondition_names = [\"import\", \"default\"]\n\n[[forbidden]]\nname = \"not-to-unresolvable\"\nseverity = \"error\"\nto = { could_not_resolve = true }\n";
    let found = custom_condition_unresolvable("explicit", Some(toml));
    assert_eq!(found, ["pkg/util"], "{found:?}");
}

#[test]
fn custom_conditions_are_per_tsconfig() {
    let dir = std::env::temp_dir().join(format!("detangle-custom-conditions-two-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let write = |path: &str, text: &str| {
        let f = dir.join(path);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    };
    write("package.json", r#"{"name":"r","private":true}"#);
    for (pkg, cond) in [("pa", "ca"), ("pb", "cb")] {
        write(&format!("node_modules/{pkg}/package.json"), &format!(r#"{{"name":"{pkg}","exports":{{"./*":{{"{cond}":"./*.ts","default":"./out/*.js"}}}}}}"#));
        write(&format!("node_modules/{pkg}/x.ts"), "export const x = 1;\n");
        write(&format!("{cond}/tsconfig.json"), &format!(r#"{{"compilerOptions":{{"customConditions":["{cond}"]}}}}"#));
        write(&format!("{cond}/f.ts"), "import { x } from 'pa/x';\nimport { x as y } from 'pb/x';\nexport const f = [x, y];\n");
    }
    let out = Command::new(env!("CARGO_BIN_EXE_detangle")).args(["check", "-f", "json"]).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let found: Vec<(String, String)> = v
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["rule"] == "not-to-unresolvable")
        .map(|x| (x["from"].as_str().unwrap().into(), x["to"].as_str().unwrap().into()))
        .collect();
    std::fs::remove_dir_all(&dir).unwrap();
    // Each tsconfig's condition resolves its own package and not the other's.
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(found.contains(&("ca/f.ts".into(), "pb/x".into())), "{found:?}");
    assert!(found.contains(&("cb/f.ts".into(), "pa/x".into())), "{found:?}");
}
