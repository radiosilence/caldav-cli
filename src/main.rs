use caldav_cli::models::Output;
use caldav_cli::{commands, mcp};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use std::io;
use tracing_subscriber::EnvFilter;

/// Time-window flags, shared by every read command.
#[derive(Args, Debug, Clone, Default)]
struct RangeOpts {
    /// Window start (ISO 8601, YYYY-MM-DD, `today`, `tomorrow`, or `+2d`).
    /// Defaults to the start of today.
    #[arg(long)]
    start: Option<String>,

    /// Window end. Takes precedence over --days.
    #[arg(long)]
    end: Option<String>,

    /// Number of days from --start. Default 7.
    #[arg(long)]
    days: Option<i64>,

    /// IANA timezone for interpreting naive dates, e.g. Europe/London.
    #[arg(long)]
    tz: Option<String>,
}

impl From<&RangeOpts> for commands::RangeArgs {
    fn from(o: &RangeOpts) -> Self {
        Self {
            start: o.start.clone(),
            end: o.end.clone(),
            days: o.days,
            tz: o.tz.clone(),
        }
    }
}

/// Event-shaped flags, shared by `create` and `update`.
#[derive(Args, Debug, Clone, Default)]
struct EventOpts {
    /// Event title
    #[arg(long)]
    summary: Option<String>,

    /// Start time (ISO 8601, `YYYY-MM-DD [HH:MM]`, `tomorrow`, `+2h`)
    #[arg(long)]
    start: Option<String>,

    /// End time. Mutually exclusive with --duration.
    #[arg(long, conflicts_with = "duration")]
    end: Option<String>,

    /// Length in minutes, as an alternative to --end
    #[arg(long)]
    duration: Option<i64>,

    /// Make this an all-day event
    #[arg(long)]
    all_day: bool,

    /// IANA timezone for naive start/end values, e.g. Europe/London
    #[arg(long)]
    tz: Option<String>,

    /// Long-form notes
    #[arg(long)]
    description: Option<String>,

    /// Where the event happens
    #[arg(long)]
    location: Option<String>,

    /// A link to attach to the event
    #[arg(long)]
    url: Option<String>,

    /// CONFIRMED, TENTATIVE, or CANCELLED
    #[arg(long)]
    status: Option<String>,

    /// Recurrence rule, e.g. FREQ=WEEKLY;BYDAY=MO
    #[arg(long)]
    recurrence: Option<String>,

    /// Attendee as `email` or `Name <email>` (repeatable). On update, omitting
    /// this keeps the existing attendees.
    #[arg(long = "attendee", action = clap::ArgAction::Append)]
    attendees: Vec<String>,

    /// Category/tag (repeatable). On update, omitting this keeps the existing ones.
    #[arg(long = "category", action = clap::ArgAction::Append)]
    categories: Vec<String>,
}

impl From<&EventOpts> for commands::EventArgs {
    fn from(o: &EventOpts) -> Self {
        Self {
            summary: o.summary.clone(),
            start: o.start.clone(),
            end: o.end.clone(),
            duration: o.duration,
            all_day: o.all_day,
            tz: o.tz.clone(),
            description: o.description.clone(),
            location: o.location.clone(),
            url: o.url.clone(),
            status: o.status.clone(),
            recurrence: o.recurrence.clone(),
            attendees: o.attendees.clone(),
            categories: o.categories.clone(),
        }
    }
}

#[derive(Parser)]
#[command(name = "caldav-cli")]
#[command(version, about = "CLI for CalDAV calendars (iCloud by default)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Store and verify CalDAV credentials
    Auth {
        /// Account username — an email address for iCloud and Fastmail
        #[arg(long)]
        username: String,

        /// App-specific password. If omitted, it is read from stdin so it
        /// doesn't appear in `ps`, shell history, or the environment.
        #[arg(long)]
        app_password: Option<String>,

        /// CalDAV base URL. Defaults to iCloud.
        #[arg(long)]
        server_url: Option<String>,
    },

    /// List calendars on the account
    Calendars,

    /// List events in a time window
    List {
        /// Calendar name or id (default: all calendars)
        #[arg(short, long)]
        calendar: Option<String>,

        #[command(flatten)]
        range: RangeOpts,

        /// Maximum events to return
        #[arg(short, long, default_value = "100")]
        limit: usize,

        /// Return recurring series as their master event rather than expanding
        /// each occurrence in the window
        #[arg(long)]
        no_expand: bool,
    },

    /// Today's events — shorthand for `list --days N`
    Agenda {
        /// Days to cover, starting today
        #[arg(long, default_value = "1")]
        days: i64,

        /// Calendar name or id (default: all calendars)
        #[arg(short, long)]
        calendar: Option<String>,

        /// IANA timezone, e.g. Europe/London
        #[arg(long)]
        tz: Option<String>,

        /// Maximum events to return
        #[arg(short, long, default_value = "100")]
        limit: usize,
    },

    /// Get a single event by UID
    Get {
        /// Event UID
        event_id: String,

        /// Restrict the lookup to one calendar
        #[arg(short, long)]
        calendar: Option<String>,
    },

    /// Search events by text
    Search {
        /// Text to match against title, notes, location, categories, attendees
        query: String,

        /// Calendar name or id (default: all calendars)
        #[arg(short, long)]
        calendar: Option<String>,

        #[command(flatten)]
        range: RangeOpts,

        /// Maximum results
        #[arg(short, long, default_value = "50")]
        limit: usize,
    },

    /// Create an event
    Create {
        /// Calendar name or id (default: first writable calendar)
        #[arg(short, long)]
        calendar: Option<String>,

        #[command(flatten)]
        event: EventOpts,
    },

    /// Update an event — only the fields you pass are changed
    Update {
        /// Event UID
        event_id: String,

        /// Restrict the lookup to one calendar
        #[arg(short, long)]
        calendar: Option<String>,

        #[command(flatten)]
        event: EventOpts,
    },

    /// Delete an event
    Delete {
        /// Event UID
        event_id: String,

        /// Restrict the lookup to one calendar
        #[arg(short, long)]
        calendar: Option<String>,

        /// Skip confirmation
        #[arg(short = 'y', long)]
        yes: bool,
    },

    /// Show busy windows over a range
    FreeBusy {
        #[command(flatten)]
        range: RangeOpts,
    },

    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },

    /// Run as MCP (Model Context Protocol) server for Claude integration
    Mcp {
        /// Serve over streamable HTTP on this address (e.g. 127.0.0.1:8080)
        /// instead of stdio. In HTTP mode the credentials are read from the
        /// `X-CalDAV-*` headers per request; over stdio they come from config.
        #[arg(long, value_name = "ADDR")]
        http: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Auth {
            username,
            app_password,
            server_url,
        } => {
            let resolved = match app_password {
                Some(p) => Ok(p),
                None => commands::read_password_from_stdin(),
            };
            match resolved {
                Ok(password) => commands::auth(server_url.as_deref(), &username, &password).await,
                Err(e) => Err(e),
            }
        }

        Commands::Calendars => commands::list_calendars().await,

        Commands::List {
            calendar,
            range,
            limit,
            no_expand,
        } => commands::list_events(calendar.as_deref(), &(&range).into(), limit, !no_expand).await,

        Commands::Agenda {
            days,
            calendar,
            tz,
            limit,
        } => {
            let range = commands::RangeArgs {
                days: Some(days),
                tz,
                ..Default::default()
            };
            commands::list_events(calendar.as_deref(), &range, limit, true).await
        }

        Commands::Get { event_id, calendar } => {
            commands::get_event(&event_id, calendar.as_deref()).await
        }

        Commands::Search {
            query,
            calendar,
            range,
            limit,
        } => commands::search_events(&query, calendar.as_deref(), &(&range).into(), limit).await,

        Commands::Create { calendar, event } => {
            commands::create_event(calendar.as_deref(), &(&event).into()).await
        }

        Commands::Update {
            event_id,
            calendar,
            event,
        } => commands::update_event(&event_id, calendar.as_deref(), &(&event).into()).await,

        Commands::Delete {
            event_id,
            calendar,
            yes,
        } => {
            if !yes {
                eprintln!("Delete event {}? Use -y to confirm.", event_id);
                std::process::exit(1);
            }
            commands::delete_event(&event_id, calendar.as_deref()).await
        }

        Commands::FreeBusy { range } => commands::free_busy(&(&range).into()).await,

        Commands::Completions { shell } => {
            generate(shell, &mut Cli::command(), "caldav-cli", &mut io::stdout());
            return;
        }

        Commands::Mcp { http } => match http {
            Some(addr) => mcp::run_http_server(&addr).await,
            None => mcp::run_server().await,
        },
    };

    if let Err(e) = result {
        Output::<()>::error(e.to_string()).print();
        std::process::exit(1);
    }
}
