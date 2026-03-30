# Instalation quick guide

Welcome, this "quick" guide will help you install the package directly from Github.

Just select below a section that fits your current question/concerns.

**Summary**

`runpy` Rust crate:

- [Cargo](#installing-the-rust-package)

`runpyrs` Python package:

- [With uv](#using-uv-package-manager)
- [With pip](#using-pip)

[Considerations](#considerations)

## The easy way
<!-- TODO: stop procrastinating and actually publish it -->
> THE PACKAGES ARE NOT AVAILABLE IN THE BELOW REGISTRIES YET!!

Just get it from [crates.io]() or [pypi]() <br>
Unless.. you really want some very specific version/commit not published.

## Installing the Rust package

It is pretty straigh forward as `Cargo` already has fetures for this purpose. <br>
Simple as passing the Github repo url to `Cargo`

```bash
cargo add --git https://github.com/dark1zinn/runpy -p runpy
```

This will simply download the code get the package code in it and add to your project

Easy right? Now if you want some more specific version or the latest commits... <br>
`Cargo` also has you covered!

```bash
# From a specific branch
cargo add --git https://github.com/dark1zinn/runpy -p runpy --branch dev

# From a specific tag
cargo add --git https://github.com/dark1zinn/runpy -p runpy --tag v0.1.0

# From a specific commit (pass in the commit hash)
cargo add --git https://github.com/dark1zinn/runpy -p runpy --rev eaa597be0249465de28c1b422a2371bc7dddc69e
```

So far that' all I know `Cargo` offers

## Installing the Python package

### Using `uv` package manager

Just as good as using cargo, pretty straigh forward as well

```bash
# Here we MUST specify the subdirectory
uv add "runpyrs @ git+https://github.com/dark1zinn/runpy#subdirectory=worker"
```

Same as cargo, will download the code, get and add the package <br>
And for specific version/commit

```bash
# From a branch
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker#branch=dev"

# By Tag
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker@v0.1.0"

# By Commit Hash
uv add "git+https://github.com/dark1zinn/runpy#subdirectory=worker@eaa597be0249465de28c1b422a2371bc7dddc69e"
```

### Using `pip`

Better than I expected, but still with some quirks

```bash
# We MUST specify the subdirectory so it can actually find the package
pip install "runpyrs @ git+https://github.com/dark1zinn/runpy.git#subdirectory=worker"
```

> A note for NixOS users: <br>
> When using pip inside a Nix shell, ensure you are in an activated venv. If you try to run pip install against the Nix-store Python, it will fail with a "Read-only file system" error.

While doing pretty much the same as `uv`, it is much slower, can cause to re-clone the repo and... <br>
Unlike `uv` that manages the `pyproject.toml` automagically here you have to manually add it to your `requirements.txt`

```
# requirements.txt
runpyrs @ git+https://github.com/dark1zinn/runpy.git#subdirectory=worker
```

And yes it can get more specific versions/commits

```bash
# By Branch
pip install "git+https://github.com/dark1zinn/runpy.git@develop"

# By Tag
pip install "git+https://github.com/dark1zinn/runpy.git@v0.0.4"

# By Commit Hash
pip install "git+https://github.com/dark1zinn/runpy.git@a1b2c3d4e5"
```

## Considerations

This brief documentation was built upon my current knowledge and some research, thus this might not be 100% up to date or be incorrect <br>
Pull requests enhancing the documentation are welcome!
