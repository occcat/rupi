use rupi_ai::ThinkingLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

#[derive(Debug, Clone)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ThinkingLevel>,
    pub continue_session: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_dir: Option<String>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Vec<String>,
    pub no_tools: bool,
    pub print: bool,
    pub no_skills: bool,
    pub skills: Vec<String>,
    pub no_context_files: bool,
    pub list_models: bool,
    pub verbose: bool,
    pub messages: Vec<String>,
    pub diagnostics: Vec<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            api_key: None,
            base_url: None,
            system_prompt: None,
            append_system_prompt: Vec::new(),
            thinking: None,
            continue_session: false,
            resume: false,
            help: false,
            version: false,
            mode: None,
            name: None,
            no_session: false,
            session: None,
            session_dir: None,
            tools: None,
            exclude_tools: Vec::new(),
            no_tools: false,
            print: false,
            no_skills: false,
            skills: Vec::new(),
            no_context_files: false,
            list_models: false,
            verbose: false,
            messages: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

pub fn parse_args(argv: Vec<String>) -> Args {
    let mut result = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            result.messages.extend(argv[i + 1..].iter().cloned());
            break;
        } else if arg == "--help" || arg == "-h" {
            result.help = true;
        } else if arg == "--version" || arg == "-v" {
            result.version = true;
        } else if arg == "--mode" && i + 1 < argv.len() {
            i += 1;
            result.mode = match argv[i].as_str() {
                "json" => Some(Mode::Json),
                "rpc" => Some(Mode::Rpc),
                _ => Some(Mode::Text),
            };
        } else if arg == "--continue" || arg == "-c" {
            result.continue_session = true;
        } else if arg == "--resume" || arg == "-r" {
            result.resume = true;
        } else if arg == "--provider" && i + 1 < argv.len() {
            i += 1;
            result.provider = Some(argv[i].clone());
        } else if arg == "--model" && i + 1 < argv.len() {
            i += 1;
            result.model = Some(argv[i].clone());
        } else if arg == "--api-key" && i + 1 < argv.len() {
            i += 1;
            result.api_key = Some(argv[i].clone());
        } else if arg == "--base-url" && i + 1 < argv.len() {
            i += 1;
            result.base_url = Some(argv[i].clone());
        } else if arg == "--system-prompt" && i + 1 < argv.len() {
            i += 1;
            result.system_prompt = Some(argv[i].clone());
        } else if arg == "--append-system-prompt" && i + 1 < argv.len() {
            i += 1;
            result.append_system_prompt.push(argv[i].clone());
        } else if (arg == "--name" || arg == "-n") && i + 1 < argv.len() {
            i += 1;
            result.name = Some(argv[i].clone());
        } else if arg == "--no-session" {
            result.no_session = true;
        } else if arg == "--session" && i + 1 < argv.len() {
            i += 1;
            result.session = Some(argv[i].clone());
        } else if arg == "--session-dir" && i + 1 < argv.len() {
            i += 1;
            result.session_dir = Some(argv[i].clone());
        } else if (arg == "--tools" || arg == "-t") && i + 1 < argv.len() {
            i += 1;
            result.tools = Some(
                argv[i]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            );
        } else if (arg == "--exclude-tools" || arg == "-xt") && i + 1 < argv.len() {
            i += 1;
            result.exclude_tools = argv[i]
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = true;
        } else if arg == "--thinking" && i + 1 < argv.len() {
            i += 1;
            result.thinking = ThinkingLevel::parse(&argv[i]);
        } else if arg == "--print" || arg == "-p" {
            result.print = true;
            if i + 1 < argv.len() && !argv[i + 1].starts_with('-') {
                i += 1;
                result.messages.push(argv[i].clone());
            }
        } else if arg == "--skill" && i + 1 < argv.len() {
            i += 1;
            result.skills.push(argv[i].clone());
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = true;
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = true;
        } else if arg == "--list-models" {
            result.list_models = true;
        } else if arg == "--verbose" {
            result.verbose = true;
        } else if arg.starts_with('-') {
            result
                .diagnostics
                .push(format!("unknown flag: {arg}"));
        } else {
            result.messages.push(arg.clone());
        }
        i += 1;
    }
    result
}

pub fn print_help() {
    println!(
        "\
rupi — Rust port of the Pi coding agent harness

Usage:
  rupi [options] [prompt]
  rupi -p \"explain this repo\"
  rupi --provider anthropic --model claude-sonnet-4-5

Modes:
  (default)     Interactive REPL with slash commands
  -p, --print   Single-turn print mode
  --mode json   JSONL event stream
  --mode rpc    JSONL RPC over stdin/stdout

Session:
  -c, --continue          Continue the latest session
  -r, --resume            Pick / load a session
  --session <path>        Load a session file
  --session-dir <dir>     Session directory
  --no-session            Do not persist
  -n, --name <name>       Session name

Model:
  --provider <name>       openai | anthropic | google | openrouter | openai-compat | faux
  --model <id>
  --api-key <key>
  --base-url <url>
  --thinking <level>      off|minimal|low|medium|high|xhigh|max
  --list-models

Tools / harness:
  -t, --tools <list>      Comma-separated allowlist
  -xt, --exclude-tools
  -nt, --no-tools
  --skill <path>          Extra skill path (repeatable)
  -ns, --no-skills
  -nc, --no-context-files
  --system-prompt <text>
  --append-system-prompt <text>

Slash commands (interactive):
  /help  /model  /mcp  /skills  /memory  /compact  /session  /review  /quit

Environment:
  ANTHROPIC_API_KEY  OPENAI_API_KEY  GEMINI_API_KEY  OPENROUTER_API_KEY
  RUPI_PROVIDER  RUPI_MODEL  RUPI_BASE_URL  RUPI_HOME
"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_print_prompt() {
        let args = parse_args(vec!["-p".into(), "hello world".into(), "--provider".into(), "faux".into()]);
        assert!(args.print);
        assert_eq!(args.messages, vec!["hello world"]);
        assert_eq!(args.provider.as_deref(), Some("faux"));
    }
}
