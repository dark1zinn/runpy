#!/bin/sh
# Build both packages the way a consumer receives them and use them from
# outside the checkout:
#
# - `cargo package` (which also verifies the crate builds alone), then a
#   temporary binary crate that depends on the packaged sources;
# - `uv build --no-sources` (sdist, then a wheel built from that sdist), then
#   fresh environments that install the wheel and the sdist and check the
#   import location, `py.typed`, and the license file.
#
# Set PACKAGE_SMOKE_ALLOW_DIRTY=1 to package uncommitted changes locally.
set -eu

repo=$(git rev-parse --show-toplevel)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
python_version=${PACKAGE_SMOKE_PYTHON:-3.10}
cd "$repo"

# Each package ships its own copy of the repository license.
cmp LICENSE manager/LICENSE
cmp LICENSE worker/LICENSE

echo '== Rust crate'
cargo package -p runpy --locked ${PACKAGE_SMOKE_ALLOW_DIRTY:+--allow-dirty}
version=$(cargo metadata --format-version 1 --no-deps --locked |
    python3 -c 'import json, sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "runpy"))')
mkdir "$work/crate"
tar -xzf "target/package/runpy-$version.crate" -C "$work/crate"
test -f "$work/crate/runpy-$version/LICENSE"

mkdir -p "$work/consumer/src"
cat >"$work/consumer/Cargo.toml" <<EOF
[package]
name = "runpy-consumer"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
runpy = { path = "../crate/runpy-$version" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[workspace]
EOF
cat >"$work/consumer/src/main.rs" <<'EOF'
use runpy::{Manager, PoolConfig, RunpyError};

#[tokio::main]
async fn main() {
    let _ = PoolConfig::default().max_workers(1);
    let scripts = std::env::temp_dir();
    match Manager::builder(&scripts).uv_path("/nonexistent/uv").build().await {
        Err(RunpyError::UvUnavailable { .. }) => println!("packaged runpy works"),
        Err(other) => panic!("unexpected error: {other}"),
        Ok(_) => panic!("a missing uv must be rejected"),
    }
}
EOF
# Start from the workspace lock so dependency versions match what was tested.
cp Cargo.lock "$work/consumer/Cargo.lock"
CARGO_TARGET_DIR="$repo/target/package-smoke" cargo run --quiet --manifest-path "$work/consumer/Cargo.toml"

echo '== Python package'
uv build --package runpyrs --no-sources --out-dir "$work/dist"
check='
import importlib.metadata, pathlib, sys
import runpyrs
location = pathlib.Path(runpyrs.__file__).resolve()
assert not location.is_relative_to(pathlib.Path(sys.argv[1])), location
assert (location.parent / "py.typed").is_file(), "py.typed missing"
files = importlib.metadata.distribution("runpyrs").files or []
assert any(f.name == "LICENSE" for f in files), "license file missing"
assert issubclass(runpyrs.AsyncWorker, object) and callable(runpyrs.RunScript)
print("installed", location)
'
for artifact in "$work"/dist/*.whl "$work"/dist/*.tar.gz; do
    environment="$work/env-$(basename "$artifact")"
    uv venv --quiet --python "$python_version" "$environment"
    uv pip install --quiet --python "$environment/bin/python" "$artifact"
    (cd "$work" && "$environment/bin/python" -c "$check" "$repo")
done
