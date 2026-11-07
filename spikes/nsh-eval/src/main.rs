use nsh_core::{ApproxTextEngine, MountBatch};
use nsh_engine::{Bundle, Engine, EngineConfig, HostChannel, MountSink, parse_capabilities};
use serde_json::{Value, json};
use std::{sync::mpsc::{self, SyncSender}, time::{Duration, Instant}};

struct Sink(SyncSender<String>);
impl MountSink for Sink {
    fn submit(&self, _: MountBatch) {}
    fn report_error(&self, message: &str) { let _ = self.0.try_send(message.to_owned()); }
}

fn config(source: &str, host: HostChannel) -> EngineConfig {
    let mut bundle = Bundle::source(source.to_owned());
    let grants = parse_capabilities(r#"{"host":["editor.handshake","editor.document.readRange","editor.eval.result"]}"#).expect("explicit grants; unlisted capabilities denied");
    bundle.policy.declared = Some(grants.clone());
    bundle.policy.granted = Some(grants);
    let mut config = EngineConfig::new(bundle);
    config.host = host;
    config.limits.memory_bytes = 64 << 20;
    config.limits.load_budget = Duration::from_millis(200);
    config.limits.call_budget = Duration::from_millis(100);
    config
}

fn command_roundtrip() -> Result<Value, Box<dyn std::error::Error>> {
    let host = HostChannel::new();
    host.handle("editor.handshake", |call| call.resolve(json!({"editorApiVersion":1,"offsetEncoding":"document-utf8","integerEncoding":"decimal-string"})));
    // A real registered handler must remain inaccessible without its grant.
    host.handle("editor.notGranted", |call| call.resolve(json!("permission bypass")));
    host.handle("editor.document.readRange", |call| {
        if call.data() != &json!({"documentId":"probe","revision":"1","start":"0","end":"6"}) {
            call.reject("STALE_REVISION or INVALID_RANGE");
        } else { call.resolve(json!({"text":"hello!","revision":"1"})); }
    });
    let (tx, rx) = mpsc::sync_channel(1);
    host.handle("editor.eval.result", move |mut call| { let _ = tx.try_send(call.take_data()); });
    let (errors, error_rx) = mpsc::sync_channel(8);
    let source = r#"
        import { host } from 'nsh';
        const api = await host.call('editor.handshake');
        const text = await host.call('editor.document.readRange', {documentId:'probe', revision:'1', start:'0', end:'6'});
        let denied = false;
        try { await host.call('editor.notGranted'); } catch (_) { denied = true; }
        host.post('editor.eval.result', {api, transformed:text.text.toUpperCase(), denied});
    "#;
    let started = Instant::now();
    let engine = Engine::start(config(source, host), || ApproxTextEngine, Sink(errors))?;
    let result = rx.recv_timeout(Duration::from_secs(5));
    let elapsed = started.elapsed();
    drop(engine);
    match result {
        Ok(value) if value["transformed"] == "HELLO!" && value["denied"] == true => {
            println!("{}", json!({"scenario":"headless-command","elapsed_us":elapsed.as_micros(),"result":value}));
            Ok(value)
        }
        other => Err(format!("command failed: {other:?}; errors: {:?}", error_rx.try_iter().collect::<Vec<_>>()).into()),
    }
}

fn fault(source: &str, label: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (tx, rx) = mpsc::sync_channel(8);
    let start = Instant::now();
    let engine = Engine::start(config(source, HostChannel::new()), || ApproxTextEngine, Sink(tx))?;
    let error = rx.recv_timeout(Duration::from_secs(3))?;
    let stop = Instant::now();
    drop(engine);
    println!("{}", json!({"scenario":label,"error":error,"error_latency_us":start.elapsed().as_micros(),"stop_us":stop.elapsed().as_micros()}));
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    command_roundtrip()?;
    fault("throw new Error('intentional-evaluation-failure')", "script-throw")?;
    fault("while (true) {}", "bounded-loop")?;
    command_roundtrip()?;
    println!("{}", json!({"scenario":"close-reopen","passed":true,"scope":"headless-only; not an editor plugin integration"}));
    Ok(())
}
