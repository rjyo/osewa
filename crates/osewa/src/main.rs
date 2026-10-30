//! The single osewa binary: the server, the per-session holder, enrollment,
//! and the client commands. Same command line as the Go osewa.

use std::process::ExitCode;

const USAGE: &str = r#"usage: osewa <command> [args]

host commands:
  server [--daemon] [--listen :7322] [--name N] [--no-compress] [--no-forward] [--no-exec] [--no-quic]
                                       serve sessions to enrolled devices; --daemon
                                       detaches (log in ~/.local/state/osewa/server.log);
                                       --no-forward and --no-exec refuse port forwarding
                                       and commands without a terminal
  service install [server flags] | uninstall
                                       run the server as a systemd/launchd user service
  enroll [--port N]                    add a device (run over ssh by login)
  holder ...                           one session's process (started by server)

client commands:
  login <ssh-target> [--name N] [--addr host:port] [--device N] [--no-install] [--service] [--no-check]
  ls <host>
  new <host> [-- argv...]
  attach [--take] [--quiet] [--predict auto|always|never] <host> [id]
                                       ~. detaches (after a newline); while viewing,
                                       Ctrl-T takes ownership
  kill <host> <id>
  exec [--cwd dir] [--env K=V]... <host> -- argv...
                                       run a command on the host without a terminal;
                                       stdin, stdout and stderr pass through, and osewa
                                       exits with its code
  update <host>                        put this build on a host over osewa (no ssh)
                                       and restart its server with it
  forward <host> [bind:]port[:host:hostport]...
                                       forward local ports through the host, as ssh -L
                                       (bind defaults to 127.0.0.1, the target to the
                                       host's localhost at the same port) until Ctrl-C
  version
"#;

/// Set at release time from OSEWA_VERSION.
const VERSION: &str = match option_env!("OSEWA_VERSION") {
    Some(v) => v,
    None => "dev",
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(run(&args) as u8)
}

fn run(args: &[String]) -> i32 {
    let Some((cmd, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        return 2;
    };
    match cmd.as_str() {
        "holder" => osewa_holder::main(rest),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            0
        }
        "version" | "--version" => {
            println!(
                "osewa {VERSION} (protocol {}, ghostty {}, snapshot v{})",
                osewa_wire::VERSION,
                osewa_ghostty::COMMIT,
                osewa_ghostty::SNAPSHOT_VERSION
            );
            0
        }
        _ => {
            eprintln!("osewa: {cmd} is not in the Rust build yet");
            eprint!("{USAGE}");
            2
        }
    }
}
