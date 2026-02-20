# RUNPY

 A Rust crate that spawns a Python process, passes a script to it etc...
 Basically Rust has controll over the Python process, then can make it execute some scripts and retrieve data.

## Why tho?

 Why not?
 You see, Python is a super simple language, however not as fast and scalable as Rust.

 For example, let's say we want to build a scraping web server, where we can make a request at a endpoint to schedule a scraping job, if we do it all in Python we are tied to the singlethreaded capacities of Python, we could make use of threads in it, but the complexity just isn't valid the effort.

 Then why not make the web server in Rust for example, then let it manage Python processes for a scraper, because Python provide the syntax simplicity to write scraper scripts while Rust manages parallel processes and handle the web server part.

 The effort to write the scraper in Rust just isn't valid as well since Rust has a lot of complexity over it and a scraper job is slow anyway so writing it in Rust would only pay off for really specific situations.

## Would you like to test it?

 Here's a simple breakdown for you to quickly test it:

- First clone this repository (duh)
- Alright, once your cwd (at the terminal, text-editor, whatever), `cd py-worker` then `python -m venv .venv` (yeah you need Python, who would've guessed).
- Now just run `pip install -e .`, and the Python part is done.
- Expectating that you've already heve Rust/Cargo (if you don't, wth you even doing here?), at the root of this project just `cargo run -p manager`.

 What is expected to be printed to your terminal is the follwoing:

```bash
   Compiling manager v0.1.0 (/home/dark1zin/repos/2-Personal-projects/runpy/manager)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.41s
     Running `target/debug/manager`
Worker listening on /tmp/rust_py.sock
Received HTML: <html><title>Hello from Rust!</title><body><a href...
Python says: ScrapingResponse { status: "success", title: "Hello from Python!", links_count: 1 }
```

 Doesn't appear like that or a error have occurred?

- Open a issue.
- Include what the hell is your operating system, architecture and stuff.
- Include a screenshot or gist of the output you got.
- Also include the steps you did for this error to happen, otherwise how am I supposed to know what the heck you did?
- If applicable, a shout out for some random user on Github.

## With that said, contributions are welcome!

 Feel free to help on this project by forking and opening PRs. <br/>
 Whatever you feel like would be a good addition for this project. <br/>
 PRs that aim to improve stability, reliability, etc will be prioritized. <br/>

> With ❤️ @dark1zinn
