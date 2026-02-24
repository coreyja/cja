use maud::{DOCTYPE, Markup, PreEscaped, html};

pub fn landing_page() -> Markup {
    base_layout("CJA — Cron, Jobs, and Axum", &landing_content())
}

fn base_layout(title: &str, content: &Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="description" content="CJA is a Rust web framework with database-backed background jobs, cron scheduling, and a production-ready Axum server.";
                title { (title) }
                link rel="icon" type="image/svg+xml" href="/favicon.svg";
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="preconnect" href="https://fonts.gstatic.com" crossorigin;
                link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Quicksand:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap";
                link rel="stylesheet" href="/style.css";
            }
            body {
                (content)
            }
        }
    }
}

fn landing_content() -> Markup {
    html! {
        (hero_section())
        (features_section())
        (code_showcase_section())
        (getting_started_section())
        (footer_section())
    }
}

fn hero_section() -> Markup {
    html! {
        section class="hero" {
            div class="container" {
                div class="hero-inner" {
                    h1 class="hero-title" {
                        span class="accent" { "CJA" }
                    }
                    p class="hero-subtitle" { "Cron, Jobs, and Axum" }
                    p class="hero-tagline" {
                        "A Rust web framework that handles the boring-but-critical infrastructure so you can focus on your app."
                    }
                    div class="hero-code" {
                        (code_block(r#"#[derive(Clone)]
struct App { db: PgPool, cookie_key: CookieKey }

impl AppState for App {
    fn db(&self) -> &PgPool { &self.db }
    fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
}

async fn run() -> Result<()> {
    let app = App::from_env().await?;
    let tasks = vec![
        NamedTask::spawn("server", run_server(routes(app.clone()))),
        NamedTask::spawn("jobs", job_worker(app, Jobs)),
    ];
    wait_for_first_error(tasks).await
}"#, "rust"))
                    }
                    div class="hero-cta" {
                        a class="btn btn-primary" href="#getting-started" { "Get Started" }
                        a class="btn btn-secondary" href="/docs/cja/index.html" { "Documentation" }
                    }
                }
            }
        }
    }
}

fn features_section() -> Markup {
    html! {
        section id="features" {
            div class="container" {
                div class="section-heading" {
                    h2 { "What's in the box" }
                }
                div class="feature-grid" {
                    (feature_card(
                        "Background Jobs",
                        "Database-backed persistence with retry and exponential backoff. Dead letter queue for failed jobs. Cancellation tokens for graceful shutdown.",
                        r#"#[derive(Debug, Serialize, Deserialize, Clone)]
struct SendEmail { to: String }

#[async_trait]
impl Job<AppState> for SendEmail {
    const NAME: &'static str = "SendEmail";
    async fn run(&self, state: AppState) -> Result<()> {
        // your logic here
        Ok(())
    }
}"#,
                        &jobs_icon(),
                    ))
                    (feature_card(
                        "Cron Scheduling",
                        "Interval or cron expression scheduling with timezone awareness. Distributed locking ensures single execution across instances.",
                        r#"let mut registry = CronRegistry::new();

registry.register_job(
    CleanupJob,
    Some("Remove expired sessions"),
    Duration::from_secs(3600),
);

registry.register_with_cron(
    "daily-report", None,
    "0 0 9 * * *",  // 9am daily
    |state, _| Box::pin(async move {
        generate_report(state).await
    }),
)?;"#,
                        &cron_icon(),
                    ))
                    (feature_card(
                        "Production Server",
                        "Axum-based HTTP server with cookie-based DB-backed sessions, structured tracing with OpenTelemetry and Sentry, and zero-downtime reloading.",
                        r#"fn routes(state: AppState) -> Router {
    Router::new()
        .route("/", get(home))
        .route("/api/items", post(create_item))
        .with_state(state)
}
// Sessions work as extractors:
async fn home(
    Session(session): Session<MySession>,
) -> impl IntoResponse { /* ... */ }"#,
                        &server_icon(),
                    ))
                }
            }
        }
    }
}

fn feature_card(title: &str, description: &str, code: &str, icon: &Markup) -> Markup {
    html! {
        div class="feature-card" {
            div class="feature-icon" {
                (icon)
            }
            h3 { (title) }
            p { (description) }
            (code_block(code, "rust"))
        }
    }
}

fn code_showcase_section() -> Markup {
    html! {
        section class="code-showcase" {
            div class="container" {
                div class="section-heading" {
                    h2 { "A complete app in 30 lines" }
                }
                (code_block(r#"use cja::app_state::AppState;
use cja::server::cookies::CookieKey;
use cja::jobs::Job;

#[derive(Clone)]
struct App { db: PgPool, cookie_key: CookieKey }

impl AppState for App {
    fn version(&self) -> &str { env!("CARGO_PKG_VERSION") }
    fn db(&self) -> &PgPool { &self.db }
    fn cookie_key(&self) -> &CookieKey { &self.cookie_key }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Digest;

#[async_trait]
impl Job<App> for Digest {
    const NAME: &'static str = "Digest";
    async fn run(&self, app: App) -> Result<()> {
        // send the daily digest
        Ok(())
    }
}

fn routes(app: App) -> Router {
    Router::new()
        .route("/", get(|| async { "Hello from CJA" }))
        .with_state(app)
}"#, "rust"))
            }
        }
    }
}

fn getting_started_section() -> Markup {
    html! {
        section id="getting-started" {
            div class="container" {
                div class="section-heading" {
                    h2 { "Get started" }
                }
                div class="steps" {
                    div class="step" {
                        div class="step-number" { "1" }
                        div class="step-content" {
                            h3 { "Add CJA to your project" }
                            (code_block("cargo add cja", "bash"))
                        }
                    }
                    div class="step" {
                        div class="step-number" { "2" }
                        div class="step-content" {
                            h3 { "Set up your database" }
                            (code_block("export DATABASE_URL=postgres://localhost/myapp", "bash"))
                        }
                    }
                    div class="step" {
                        div class="step-number" { "3" }
                        div class="step-content" {
                            h3 { "Run it" }
                            (code_block("cargo run", "bash"))
                        }
                    }
                }
                p class="step-note" {
                    "Requires PostgreSQL. See the "
                    a href="/docs/cja/index.html" { "full documentation" }
                    " for a complete guide."
                }
            }
        }
    }
}

fn footer_section() -> Markup {
    html! {
        footer class="site-footer" {
            div class="container" {
                div class="footer-grid" {
                    div class="footer-brand" {
                        h3 { "CJA" }
                        p { "A Rust web framework for background jobs, cron scheduling, and production HTTP servers." }
                    }
                    div class="footer-links" {
                        h4 { "Links" }
                        ul {
                            li { a href="/docs/cja/index.html" { "Documentation" } }
                            li { a href="https://github.com/coreyja/cja" { "GitHub" } }
                            li { a href="https://crates.io/crates/cja" { "Crates.io" } }
                        }
                    }
                    div class="footer-about" {
                        h4 { "About" }
                        p {
                            "Built with CJA by "
                            a href="https://coreyja.com" { "coreyja" }
                        }
                    }
                }
                div class="footer-bottom" {
                    p { "© 2026 CJA" }
                }
            }
        }
    }
}

fn code_block(code: &str, _language: &str) -> Markup {
    html! {
        div class="code-block" {
            pre {
                code { (code) }
            }
        }
    }
}

fn jobs_icon() -> Markup {
    PreEscaped(r#"<svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="6" width="20" height="12" rx="2"/><path d="M12 12h.01"/><path d="M17 12h.01"/><path d="M7 12h.01"/></svg>"#.to_string())
}

fn cron_icon() -> Markup {
    PreEscaped(r#"<svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg>"#.to_string())
}

fn server_icon() -> Markup {
    PreEscaped(r#"<svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="2" width="20" height="8" rx="2" ry="2"/><rect x="2" y="14" width="20" height="8" rx="2" ry="2"/><line x1="6" y1="6" x2="6.01" y2="6"/><line x1="6" y1="18" x2="6.01" y2="18"/></svg>"#.to_string())
}
