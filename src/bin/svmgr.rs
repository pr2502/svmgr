use clap::Parser;

#[derive(Parser, Debug)]
struct Args {
    /// If present `svmgr` starts in user mode for the given user
    #[clap(long)]
    user: Option<String>,
}

fn main() {
    let args = Args::parse();
    dbg!(args);
}
