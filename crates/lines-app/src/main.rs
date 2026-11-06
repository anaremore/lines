//! The public `lines` binary: the app with the built-in sources only (decision 0020).

fn main() -> anyhow::Result<()> {
    lines_app::run(lines_app::Extensions::new())
}
