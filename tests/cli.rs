use std::process::Command;

fn tangle(args: &[&str]) -> (String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_tangle"))
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

#[test]
fn runs_javascript_rule_configs() {
    let (out, code) = tangle(&["check", FIXTURE, "-c", "tests/fixtures/rules.config.json", "-f", "json"]);
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
    let dir = std::env::temp_dir().join(format!("tangle-init-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tangle"))
        .args(["init", dir.to_str().unwrap(), "--from", "tests/fixtures/rules.config.json"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let toml = std::fs::read_to_string(dir.join("tangle.toml")).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("6 forbidden, 0 allowed, 1 required"), "{stdout}");
    assert!(toml.contains("\"npm-bundled\""), "{toml}");
    assert!(toml.contains("path_not = \"^src/features/$1/\""), "{toml}");
}

#[test]
fn html_report_embeds_the_analysis() {
    let file = std::env::temp_dir().join(format!("tangle-report-{}.html", std::process::id()));
    let (out, code) = tangle(&["report", FIXTURE, "-o", file.to_str().unwrap()]);
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
    let (out, _) = tangle(&["graph", "tests/fixtures/aliases", "-f", "json"]);
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
            "@lib/strings -> src/lib/strings.js",              // tangle.toml alias
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
    let (out, _) = tangle(&["graph", "tests/fixtures/vite", "-f", "json"]);
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
    let (out, code) = tangle(&args);
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
fn config_env_from_tangle_toml() {
    let cfg = std::env::temp_dir().join(format!("tangle-config-env-{}.toml", std::process::id()));
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

/// Like `config_env_targets`, with extra environment variables for tangle.
fn config_env_targets_with(extra: &[&str], vars: &[(&str, &str)]) -> Vec<String> {
    let mut args = vec!["graph", "tests/fixtures/config-env", "-f", "json"];
    args.extend(extra);
    let out = Command::new(env!("CARGO_BIN_EXE_tangle"))
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
    // tangle.toml `vars` beat the shell.
    let cfg = std::env::temp_dir().join(format!("tangle-dotenv-vars-{}.toml", std::process::id()));
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
    let out = Command::new(env!("CARGO_BIN_EXE_tangle"))
        .args(["migrate", "tests/fixtures/migrate", "--dry-run"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let toml_text = String::from_utf8(out.stdout).unwrap();
    let summary = String::from_utf8(out.stderr).unwrap();
    // Nothing is written in a dry run.
    assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migrate/tangle.toml").exists());

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
    assert_eq!(options["baseline"].as_str(), Some(".tangle-baseline.json"));

    for expected in [
        "known.json  1 known violations",
        "\"deps\": \"some-dep-checker --config .deps-rules.json src\"",
        "\"cycles\": \"madge --circular --extensions ts src\"",
        "\"cycles\": \"tangle check\"",
    ] {
        assert!(summary.contains(expected), "missing {expected:?} in:\n{summary}");
    }
}

#[test]
fn migrate_writes_config_and_baseline() {
    let dir = std::env::temp_dir().join(format!("tangle-migrate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/migrate"), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
        (String::from_utf8(out.stdout).unwrap(), out.status.code().unwrap())
    };
    let (out, code) = run(&["migrate"]);
    assert_eq!(code, 0, "{out}");
    assert!(dir.join("tangle.toml").is_file() && dir.join(".tangle-baseline.json").is_file());
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

/// Migrates a fixture copy and returns the imports flagged by `tangle check`
/// as (file, specifier) — group-scope violations expanded to their imports.
fn migrated_flagged_imports(fixture: &str) -> std::collections::BTreeSet<(String, String)> {
    let dir = std::env::temp_dir().join(format!("tangle-{fixture}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(fixture), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
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
    let (out, _) = tangle(&["check", "tests/fixtures/conditions", "-c", "tests/fixtures/conditions/rules.config.cjs", "-f", "json"]);
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
    let dir = std::env::temp_dir().join(format!("tangle-ci-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    copy_dir(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/nx-options"), &dir);
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_tangle")).args(args).current_dir(&dir).env("NO_COLOR", "1").output().unwrap();
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
    let cfg = std::fs::read_to_string(dir.join("tangle.toml")).unwrap().replace("[options]", "[options]\nbaseline_stale = \"warn\"");
    std::fs::write(dir.join("tangle.toml"), cfg).unwrap();
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
