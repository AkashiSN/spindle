//! ログの既定フィルタ（P1-0）: symphonia / lofty のファイルごとの WARN を落とし、
//! spindle 自身は info

use tracing::Level;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;

struct Collect(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

impl<S: tracing::Subscriber> Layer<S> for Collect {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        self.0
            .lock()
            .unwrap()
            .push(event.metadata().target().to_owned());
    }
}

#[test]
fn default_filter_parses_and_drops_decoder_warnings_but_keeps_spindle_info() {
    let filter = EnvFilter::try_new(spindle::logging::default_filter()).unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(Collect(seen.clone()));
    let _guard = subscriber.set_default();

    tracing::event!(target: "symphonia_core::probe", Level::WARN, "unsupported");
    tracing::event!(target: "symphonia_bundle_flac::demuxer", Level::WARN, "crc");
    tracing::event!(target: "lofty::flac", Level::WARN, "padding");
    tracing::event!(target: "symphonia_core::probe", Level::ERROR, "fatal");
    tracing::event!(target: "spindle::import::scanner", Level::INFO, "スキャン完了");
    tracing::event!(target: "spindle::import::scanner", Level::WARN, "読めない");
    tracing::event!(target: "spindle::import::scanner", Level::DEBUG, "詳細");

    let seen = seen.lock().unwrap();
    assert_eq!(
        *seen,
        vec![
            "symphonia_core::probe".to_owned(),
            "spindle::import::scanner".to_owned(),
            "spindle::import::scanner".to_owned(),
        ],
        "{seen:?}"
    );
}
