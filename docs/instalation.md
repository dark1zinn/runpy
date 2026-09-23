# Installation quick guide

Welcome, this "quick" guide will help you install the package directly from Github.

Just select below a section that fits your current question/concerns.

**Summary**

`runpy` Rust crate:

- [Cargo](#installing-the-rust-package)

`runpyrs` Python package (`uv` is required):

- [With uv](#using-uv-package-manager)

[Considerations](#considerations)

## The easy way
<!-- TODO: stop procrastinating and actually publish it -->
> THE PACKAGES ARE NOT AVAILABLE IN THE BELOW REGISTRIES YET!!

Just get it from [crates.io]() or [pypi]() <br>
Unless.. you really want some very specific version/commit not published.

## Installing the Rust package

Cargo supports Git dependencies directly. The repository is a workspace, and Cargo
finds the `runpy` crate in the `manager` workspace member:

```bash
cargo add --git https://github.com/dark1zinn/runpy runpy
```

Without a Git reference, Cargo uses the latest commit on the repository's default
branch. Use `--branch`, `--tag`, or `--rev` to select another revision:

```bash
# From a specific branch
cargo add --git https://github.com/dark1zinn/runpy runpy --branch main

# From a specific tag
cargo add --git https://github.com/dark1zinn/runpy runpy --tag v0.1.0-dev.1

# From a specific commit
cargo add --git https://github.com/dark1zinn/runpy runpy --rev eaa597be0249465de28c1b422a2371bc7dddc69e
```

## Installing the Python package

### Using `uv` package manager

The Python package is in the repository's `worker` subdirectory, so the
`subdirectory` fragment is required:

```bash
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

`uv add` records the `runpyrs` dependency and its Git source in the project's
`pyproject.toml`, then updates the lockfile and environment. Use `--branch`,
`--tag`, or `--rev` to select another revision:

```bash
# From a specific branch
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker" --branch main

# From a specific tag
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker" --tag v0.1.0-dev.1

# From a specific commit
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker" --rev eaa597be0249465de28c1b422a2371bc7dddc69e
```

## Considerations

The Git dependency syntax in this guide follows the official
[Cargo `add` documentation](https://doc.rust-lang.org/cargo/commands/cargo-add.html)
and [uv Git dependency documentation](https://docs.astral.sh/uv/concepts/projects/dependencies/#git).
