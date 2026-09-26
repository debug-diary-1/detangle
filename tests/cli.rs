use std::process::Command;

fn tangle(args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_tangle"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
}

const FIXTURE: &str = "tests/fixtures/basic";

#[test]
fn check_reports_every_rule() {
    let (out, code) = tangle(&["check", FIXTURE, "-f", "json"]);
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
    let (out, _) = tangle(&["graph", FIXTURE, "-f", "json"]);
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

#[test]
fn why_prints_shortest_chain() {
    let (out, code) = tangle(&["why", "src/index.ts", "helper.test.ts", "-C", FIXTURE]);
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
    let (out, _) = tangle(&["affected", "-C", FIXTURE, "src/features/user/user.ts"]);
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
fn backreference_rules() {
    let dir = std::env::temp_dir().join(format!("tangle-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("tangle.toml");
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
    let (out, code) = tangle(&["check", FIXTURE, "-c", cfg.to_str().unwrap(), "-f", "json"]);
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
    let bl = std::env::temp_dir().join(format!("tangle-bl-{}.json", std::process::id()));
    let bl = bl.to_str().unwrap();
    assert_eq!(tangle(&["check", FIXTURE, "--write-baseline", bl]).1, 0);
    let (_, code) = tangle(&["check", FIXTURE, "--baseline", bl]);
    std::fs::remove_file(bl).unwrap();
    assert_eq!(code, 0);
}

#[test]
fn vue_svelte_angular() {
    let (out, _) = tangle(&["graph", "tests/fixtures/frameworks", "-f", "json", "--externals"]);
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
