use anyhow::anyhow;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use lazy_static::lazy_static;
use log::{error, info, trace};
use prometheus::{
    register_gauge, register_gauge_vec, register_int_gauge, Encoder, Gauge, GaugeVec, IntGauge,
    TextEncoder,
};
use std::{error::Error, future::IntoFuture, net::SocketAddr, time::Duration};
use tokio::time;
use tokio_modbus::{
    client::{rtu, Context},
    prelude::Reader,
    Slave,
};
use tokio_serial::SerialStream;

const STRINGS: [&str; 2] = ["1", "2"];
const PHASES: [&str; 3] = ["1", "2", "3"];

lazy_static! {
    static ref INVERTER_STATUS: IntGauge =
        register_int_gauge!("inverter_status", "Current state of the inverter").unwrap();
    static ref INVERTER_FAULT: IntGauge =
        register_int_gauge!("inverter_fault", "Current fault code of the inverter").unwrap();
    static ref INVERTER_ERROR: IntGauge =
        register_int_gauge!("inverter_error", "Current 4 byte error code of the inverter").unwrap();
    static ref INVERTER_WARNING: IntGauge =
        register_int_gauge!("inverter_warning", "Current error code of the inverter").unwrap();
    static ref INVERTER_POWER_PCT: IntGauge =
        register_int_gauge!("inverter_power_pct", "Current power percentage of the inverter").unwrap();
    static ref OUTPUT_POWER: Gauge =
        register_gauge!("output_power", "Current output power of the inverter in W").unwrap();
    static ref ENERGY_TODAY: Gauge =
        register_gauge!("energy_today", "Energy produced today in kWh").unwrap();
    static ref ENERGY_TOTAL: Gauge =
        register_gauge!("energy_total", "Energy produced since installation in kWh").unwrap();

    // Metrics per string of solar panels
    static ref PV_VOLTAGE: GaugeVec =
        register_gauge_vec!("pv_voltage", "Voltage of PV string in V", &["pv_string"]).unwrap();
    static ref PV_CURRENT: GaugeVec =
        register_gauge_vec!("pv_current", "Current of PV string in A", &["pv_string"]).unwrap();
    static ref PV_POWER: GaugeVec =
        register_gauge_vec!("pv_power", "Power of PV string in W", &["pv_string"]).unwrap();
    static ref PV_ENERGY_TODAY: GaugeVec =
        register_gauge_vec!("pv_energy_today", "Energy produced on this string today in kWh", &["pv_string"]).unwrap();
    static ref PV_ENERGY_TOTAL: GaugeVec =
        register_gauge_vec!("pv_energy_total", "Energy produced on this string since installation in kWh", &["pv_string"]).unwrap();

    // Metrics per grid phase
    static ref GRID_VOLTAGE: GaugeVec =
        register_gauge_vec!("grid_voltage", "Grid voltage in V", &["phase"]).unwrap();
    static ref GRID_CURRENT: GaugeVec =
        register_gauge_vec!("grid_current", "Grid output current in A", &["phase"]).unwrap();
    static ref GRID_POWER: GaugeVec =
        register_gauge_vec!("grid_power", "Grid output power in W", &["phase"]).unwrap();
    static ref INVERTER_TEMPERATURE: GaugeVec =
        register_gauge_vec!("inverter_temperature", "Inverter temperature in degrees C", &["point"]).unwrap();
}

fn assemble(val: u16) -> f64 {
    assemble2(val, 0)
}

fn combine(low: u16, high: u16) -> u32 {
    ((high as u32) << 16) | (low as u32)
}

fn assemble2(low: u16, high: u16) -> f64 {
    (combine(low, high) as f64) / 10.0
}

async fn modbus_task(tty_path: String) {
    //let tty_path = "/dev/ttyUSB0";
    let slave = Slave(0x01);

    let builder = tokio_serial::new(tty_path, 9600);
    let port = SerialStream::open(&builder).unwrap();

    let mut ctx = rtu::attach_slave(port, slave);

    let mut interval = time::interval(Duration::from_secs(15));

    let mut last_response_success = true;

    loop {
        interval.tick().await;
        tokio::select! {
            _ = modbus_get_data(&mut ctx) => {
                if !last_response_success {
                    info!("good morning, got data again");
                    last_response_success = true;
                }
            }
            _ = time::sleep(Duration::from_secs(14)) => {
                if last_response_success {
                    info!("timeout, good night");
                    last_response_success = false;
                }
                reset_metrics()
            }
        }
    }
}

fn reset_metrics() {
    INVERTER_STATUS.set(-1);
    INVERTER_POWER_PCT.set(0);
    OUTPUT_POWER.set(0.0);
    ENERGY_TODAY.set(0.0);

    for pv_str in STRINGS {
        PV_VOLTAGE.with_label_values(&[pv_str]).set(0.0);
        PV_CURRENT.with_label_values(&[pv_str]).set(0.0);
        PV_POWER.with_label_values(&[pv_str]).set(0.0);
        PV_ENERGY_TODAY.with_label_values(&[pv_str]).set(0.0);
    }

    for phase in PHASES {
        GRID_CURRENT.with_label_values(&[phase]).set(0.0);
        GRID_POWER.with_label_values(&[phase]).set(0.0);
    }
}

async fn modbus_get_data(ctx: &mut Context) -> Result<(), Box<dyn Error>> {
    trace!("Reading a sensor value");
    let rsp = ctx.read_input_registers(0, 125).await?;
    trace!("Sensor value is: {rsp:?}");

    INVERTER_STATUS.set(rsp[0].into());
    INVERTER_FAULT.set(rsp[105].into());
    INVERTER_ERROR.set(combine(rsp[107], rsp[106]).into());
    INVERTER_WARNING.set(combine(rsp[111], rsp[110]).into());
    INVERTER_POWER_PCT.set(rsp[113].into());

    OUTPUT_POWER.set(assemble(rsp[36]));
    ENERGY_TODAY.set(assemble(rsp[54]));
    ENERGY_TOTAL.set(assemble(rsp[56]));

    for (i, pv_str) in STRINGS.iter().enumerate() {
        PV_VOLTAGE
            .with_label_values(&[pv_str])
            .set(assemble(rsp[(4 * i) + 3]));
        PV_CURRENT
            .with_label_values(&[pv_str])
            .set(assemble(rsp[(4 * i) + 4]));
        PV_POWER
            .with_label_values(&[pv_str])
            .set(assemble2(rsp[(4 * i) + 6], rsp[(4 * i) + 5]));
        PV_ENERGY_TODAY
            .with_label_values(&[pv_str])
            .set(assemble2(rsp[(4 * i) + 60], rsp[(4 * i) + 59]));
        PV_ENERGY_TOTAL
            .with_label_values(&[pv_str])
            .set(assemble2(rsp[(4 * i) + 62], rsp[(4 * i) + 61]));
    }

    for (i, phase) in PHASES.iter().enumerate() {
        GRID_VOLTAGE
            .with_label_values(&[phase])
            .set(assemble(rsp[(4 * i) + 38]));
        GRID_CURRENT
            .with_label_values(&[phase])
            .set(assemble(rsp[(4 * i) + 39]));
        GRID_POWER
            .with_label_values(&[phase])
            .set(assemble2(rsp[(4 * i) + 41], rsp[(4 * i) + 40]));

        // UGLY: we happen to have 3 points
        INVERTER_TEMPERATURE
            .with_label_values(&[phase])
            .set(assemble(rsp[i + 93]));
    }
    Ok(())
    /*for i in 0..125 {
        println!("{}: {}", i, rsp[i]);
    }*/
}

async fn get_metrics() -> Result<String, AppError> {
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();

    // Gather the metrics.
    let metric_families = prometheus::gather();
    // Encode them to send.
    encoder.encode(&metric_families, &mut buffer)?;

    String::from_utf8(buffer.clone()).map_err(|e| anyhow!(e).into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    pretty_env_logger::init_timed();
    let args = std::env::args().collect::<Vec<_>>();
    let tty_path = &args[1];
    let modbus_join = tokio::spawn(modbus_task(tty_path.to_string()));

    let app = Router::new().route("/metrics", get(get_metrics));

    // 80 86 = PV
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 8086)))
        .await
        .unwrap();
    let axum_join = axum::serve(listener, app).into_future();
    tokio::select! {
       _ =  axum_join => {
           error!("axum joined");
       }
        _ = modbus_join => {
            error!("the modbus thread joined");
        }
    };
    Ok(())
}

// Make our own error that wraps `anyhow::Error`.
struct AppError(anyhow::Error);

// Tell axum how to convert `AppError` into a response.
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

// This enables using `?` on functions that return `Result<_, anyhow::Error>` to turn them into
// `Result<_, AppError>`. That way you don't need to do that manually.
impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
