#!/usr/bin/env bash
# Creates a POM inference engine plugin: the creating-pom-plugins scaffold,
# then this skill's engine overlay (supervisor, /pom/generate bridge, settings,
# engine screen, engine.lock, build-runtime.sh) wired into src/lib.rs.
#
#   bash new-engine-plugin.sh <dir>/pom-plugin-<name> --code <plugin_code> --name "<Display Name>" \
#     --engine-repo <fork git URL> --upstream <official repo URL> [--engine-commit <sha>]
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
plugin_skill="$here/../creating-pom-plugins"
overlay="$here/overlay"
dest="" code="" name="" engine_repo="" upstream="" engine_commit="0000000000000000000000000000000000000000"

die() {
  printf 'new-engine-plugin: %s\n' "$1" >&2
  exit 2
}

while (($# > 0)); do
  case "$1" in
    --code) code="${2:?}"; shift 2 ;;
    --name) name="${2:?}"; shift 2 ;;
    --engine-repo) engine_repo="${2:?}"; shift 2 ;;
    --upstream) upstream="${2:?}"; shift 2 ;;
    --engine-commit) engine_commit="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
    -*) die "unknown option: $1" ;;
    *) [[ -z "$dest" ]] || die "unexpected argument: $1"; dest="$1"; shift ;;
  esac
done
[[ -n "$dest" && -n "$code" && -n "$name" && -n "$engine_repo" && -n "$upstream" ]] \
  || die 'usage: new-engine-plugin.sh <dir>/pom-plugin-<name> --code <code> --name <name> --engine-repo <url> --upstream <url>'
[[ -f "$plugin_skill/scripts/new-plugin.sh" ]] || die "the creating-pom-plugins skill is missing next to this one"
[[ "$engine_repo$upstream" != *[\"\\\|]* ]] || die 'repository URLs may not contain quotes, backslashes or |'
[[ "$engine_commit" =~ ^[0-9a-f]{40}$ ]] || die "--engine-commit must be a full commit SHA"

bash "$plugin_skill/scripts/new-plugin.sh" "$dest" --code "$code" --name "$name" >/dev/null
upper="$(printf '%s' "$code" | tr '[:lower:]' '[:upper:]')"

cp -a "$overlay/." "$dest/"
while IFS= read -r -d '' file; do
  grep -Iq . "$file" || continue
  sed -i.bak \
    -e "s|__CODE_UPPER__|${upper}|g" \
    -e "s|__CODE__|${code}|g" \
    -e "s|__NAME__|${name}|g" \
    -e "s|__ENGINE_REPOSITORY__|${engine_repo}|g" \
    -e "s|__ENGINE_UPSTREAM__|${upstream}|g" \
    -e "s|__ENGINE_COMMIT__|${engine_commit}|g" \
    "$file"
  rm -f "$file.bak"
done < <(find "$dest" -type f ! -name '*.png' ! -path '*/node_modules/*' -print0)

# Wire the engine into the template's dispatch. Every anchor must exist: a
# template change that moves one fails here instead of producing a plugin
# that compiles without its engine.
python3 -I - "$dest" <<'PY'
import json, pathlib, sys

root = pathlib.Path(sys.argv[1])

def patch(path, edits):
    file = root / path
    text = file.read_text()
    for old, new in edits:
        if text.count(old) != 1:
            sys.exit(f"new-engine-plugin: anchor not found once in {path}: {old.strip()[:60]}")
        text = text.replace(old, new)
    file.write_text(text)

patch("src/lib.rs", [
    ("mod abi;\nmod context;\n",
     "mod abi;\nmod bridge;\nmod context;\npub mod engine;\nmod http;\npub mod launch;\nmod settings;\n"),
    ("pub struct Plugin {\n",
     "pub struct Plugin {\n    /// An error when this process has no node around it (no `POM_PLUGIN_DB`).\n"
     "    engine: Result<engine::Engine, String>,\n"),
    ("        Ok(Self {\n            context: RwLock::new(Context::default()),\n",
     "        let engine = match std::env::var_os(\"POM_PLUGIN_DB\") {\n"
     "            Some(_) => plugin_directory().and_then(engine::Engine::start),\n"
     "            None => Err(\"POM_PLUGIN_DB is not set: no POM node runs this plugin\".into()),\n"
     "        };\n"
     "        Ok(Self {\n            engine,\n            context: RwLock::new(Context::default()),\n"),
    ("            // What this plugin serves to the cluster; the dashboard draws each\n"
     "            // entry next to the machine. Empty until the plugin runs something.\n"
     "            \"host.services\" => Ok(json!({\"services\": []})),\n",
     "            // The inference service this machine offers and the\n"
     "            // architectures it runs, for the dashboard and the deploy screen.\n"
     "            \"host.services\" => match &self.engine {\n"
     "                Ok(engine) => Ok(json!({\"services\": [engine.snapshot().service()]})),\n"
     "                Err(_) => Ok(json!({\"services\": []})),\n"
     "            },\n"
     "            // `engine.*` come from this node's POM process only, through the\n"
     "            // local plugin host: no other machine and no browser reaches them.\n"
     "            \"engine.load\" => {\n"
     "                let load = engine::LoadRequest::from_json(&request[\"params\"])?;\n"
     "                self.engine()?.load(load)?;\n"
     "                Ok(json!({\"status\": \"accepted\"}))\n"
     "            }\n"
     "            \"engine.unload\" => {\n"
     "                self.engine()?.unload();\n"
     "                Ok(json!({\"status\": \"accepted\"}))\n"
     "            }\n"
     "            \"engine.status\" => Ok(self.engine()?.status_json()),\n"
     "            \"engine.upstream\" => Ok(self.engine()?.upstream_json()),\n"
     "            \"engine.link\" => Ok(self.engine()?.link_json()),\n"),
    ("            \"rpc.status\" => Ok(self.status()),\n",
     "            \"rpc.status\" => {\n"
     "                // The plugin's own status with the engine's beside it.\n"
     "                let mut status = self.status();\n"
     "                let engine = match &self.engine {\n"
     "                    Ok(engine) => engine.status_json(),\n"
     "                    Err(error) => json!({\"state\": \"error\", \"detail\": error, \"in_flight\": 0,\n"
     "                        \"runtime\": {\"ready\": false, \"version\": null}, \"seconds_in_state\": 0}),\n"
     "                };\n"
     "                if let (Some(status), Value::Object(engine)) = (status.as_object_mut(), engine) {\n"
     "                    status.extend(engine);\n"
     "                }\n"
     "                // The engine's own progress lines are for the administrator only.\n"
     "                if let (Ok(engine), true) = (&self.engine, request[\"profile\"] == \"admin\") {\n"
     "                    status[\"log\"] = json!(engine.log_tail(300));\n"
     "                }\n"
     "                Ok(status)\n"
     "            }\n"),
    ("    fn context(&self) -> Context {\n",
     "    fn engine(&self) -> Result<&engine::Engine, String> {\n"
     "        self.engine.as_ref().map_err(Clone::clone)\n"
     "    }\n\n"
     "    fn context(&self) -> Context {\n"),
    ("    #[test]\n    fn unknown_operations_fail() {\n",
     "    #[test]\n    fn engine_operations_need_a_node_and_refuse_a_bad_load() {\n"
     "        let plugin = plugin();\n"
     "        assert_eq!(plugin.query(&json!({\"operation\": \"host.services\"})).unwrap(), json!({\"services\": []}));\n"
     "        assert!(plugin.query(&json!({\"operation\": \"engine.status\"})).is_err());\n"
     "        assert!(plugin\n"
     "            .query(&json!({\"operation\": \"engine.load\", \"params\": {}}))\n"
     "            .unwrap_err()\n"
     "            .contains(\"model_path\"));\n"
     "        let status = plugin.query(&json!({\"operation\": \"rpc.status\", \"profile\": \"admin\"})).unwrap();\n"
     "        assert_eq!(status[\"state\"], \"error\");\n"
     "    }\n\n"
     "    #[test]\n    fn unknown_operations_fail() {\n"),
])

manifest_path = root / "ui/manifest.json"
manifest = json.loads(manifest_path.read_text())
manifest["menu"][0]["icon"] = "cpu"
manifest["description"] = {
    "en": f"Runs models on the {manifest['menu'][0]['label']['en']} inference engine.",
    "pt-BR": f"Executa modelos no motor de inferência {manifest['menu'][0]['label']['en']}.",
}
manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n")

release_path = root / "release/manifest.json"
release = json.loads(release_path.read_text())
release.pop("preferences", None)
release_path.write_text(json.dumps(release, indent=2, ensure_ascii=False) + "\n")

gitignore = root / ".gitignore"
lines = gitignore.read_text().splitlines()
if "/build/" not in lines:
    lines.append("/build/")
gitignore.write_text("\n".join(lines) + "\n")
PY

# The patched dispatch in the project's own style.
if command -v cargo >/dev/null 2>&1; then
  (cd "$dest" && cargo fmt >/dev/null 2>&1) || true
fi

if grep -rIl -e '__CODE' -e '__NAME__' -e '__ENGINE_' -e '__REPO__' -e '__CRATE__' "$dest" --exclude-dir=node_modules >/dev/null; then
  die 'placeholders left behind'
fi
printf 'created engine plugin %s (plugin_code=%s)\n' "$dest" "$code"
printf 'next: adapt src/launch.rs and scripts/build-runtime.sh, then the checks in SKILL.md\n'
