use std::{
	sync::Once,
	thread, time, sync::Arc,
};
use slint::{self, ComponentHandle, Weak};

use clap::Parser;
use serde::Deserialize;
use chrono::{prelude::*, Duration};
use image::{self, DynamicImage};

use tokio;
use reqwest as http;
use futures_util::TryFutureExt;

use tracing as log;
use tracing_subscriber;

use anyhow::{Result, Context};
use sys_locale;

mod ui {
	slint::include_modules!();
}

#[derive(Parser, Debug)]
#[clap(about, version)]
struct Opt {
	/// URL of the webcam website
	#[clap(default_value = "http://othcam.oth-regensburg.de/webcam")]
	base_url: reqwest::Url,

	/// Webcam name
	#[clap(default_value = "Regensburg")]
	webcam: String,

	/// Crop image with format WxH+X+Y e.g. 100x200+50+200
	#[clap(short, long)]
	crop: Option<Geometry>,

	/// Show current time
	#[clap(short = 't', long)]
	show_time: bool,

	/// Show current date
	#[clap(short = 'd', long)]
	show_date: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>>
{
	let opt = Opt::parse();

	tracing_subscriber::fmt()
		.with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
			.add_directive("camview=debug".parse()?)
			//.add_directive("hyper=debug".parse()?)
		)
		.compact()
		.init();

	let ui = ui::App::new();
	let io_task_handle = thread::Builder::new()
		.name("io-runtime".into())
		.spawn({
			let ui = ui.as_weak();
			move || io_runtime_run(ui.clone(), opt).expect("fatal error")
		})?;

	let _cleanup_task_handle = thread::Builder::new()
		.name("cleanup".into())
		.spawn({
			let ui = ui.as_weak();
			move || {
				if let Err(panic) = io_task_handle.join()
				{
					let err_str = panic.downcast::<String>()
						.map(|str_box| *str_box)
						.or_else(|panic| panic.downcast::<&str>().map(|str_box| str_box.to_string()))
						.unwrap_or_else(|panic| format!("unknown panic type: {:?}", panic));

					log::error!("PANIC! {}", err_str);
					thread::sleep(time::Duration::from_secs(1));
					ui.upgrade_in_event_loop(move |ui| {
						ui.set_bg_text("PANIC!".into());
						ui.set_status_error(true);
						ui.set_status(err_str.into());
					});
					thread::sleep(time::Duration::from_secs(10));
				}
				slint::invoke_from_event_loop(move || slint::quit_event_loop());
			}
		});

	ui.run();
	Ok(())
}

fn io_runtime_run(ui: Weak<ui::App>, opt: Opt) -> Result<()>
{
	use tokio::*;

	let rt = runtime::Builder::new_current_thread()
		.enable_all()
		.build()?;

	rt.block_on(io_run(ui, opt))?;
	rt.shutdown_timeout(time::Duration::from_secs(1));

	Ok(())
}

async fn io_run(ui: Weak<ui::App>, mut opt: Opt) -> Result<()>
{
	use tokio::*;

	opt.base_url.path_segments_mut().unwrap().push("");

	let url_base = opt.base_url.join(&opt.webcam).unwrap();
	let url_list = {
		let mut url = opt.base_url.join("include/list.php")
			.context("failed to create list URL")?;
		url.query_pairs_mut().append_pair("wc", &opt.webcam);
		url
	};

	let url_status = {
		let mut url = opt.base_url.join("include/logtail.php")
			.context("failed to create status URL")?;
		url.query_pairs_mut().append_pair("wc", "status");
		url
	};

	let client = http::Client::builder()
		.connect_timeout(time::Duration::from_secs(30))
		.timeout(time::Duration::from_secs(90))
		.pool_idle_timeout(time::Duration::from_millis(100))
		.build()
		.context("failed to create HTTP client")?;

	let img_notify = Arc::new(sync::Notify::new());
	let (err_tx, mut err_rx) = sync::mpsc::channel(8);
	let sync_task = spawn({
		let client = client.clone();
		let err_tx = err_tx.clone();
		let img_notify = img_notify.clone();
		let ui = ui.clone();
		async move {
			let mut status_ts = {
				let now = chrono::Local::now();
				let then = now - Duration::hours(1);
				then.timestamp().to_string()
			};
			loop {
				let res = update_status(&client, &url_status, &mut status_ts);
				let delay = match res.await {
					Ok(opt) => match opt {
						Some(state) => {
							let state_str: &'static str = state.into();
							log::debug!("status: {}", state_str);
							let (delay, desc) = match state {
								CamState::Ready => {
									img_notify.notify_one();
									(20, "")
								},
								CamState::Idle => (2, ""),
								_ => (2, state_str),
							};
							ui.upgrade_in_event_loop(move |ui| {
								ui.set_status_error(false);
								if desc.is_empty() {
									ui.set_status(desc.into());
								} else {
									ui.set_status(format!("Updating: {desc}...").into());
								}
							});
							delay
						},
						None => {
							2
						},
					},
					Err(err) => {
						err_tx.send(err).await
							.expect("failed to send error");
						20
					},
				};
				time::sleep(time::Duration::from_secs(delay)).await;
			}
		}
	});

	img_notify.notify_one(); // load current image
	let _img_task = spawn({
		let ui = ui.clone();
		let client = client.clone();
		async move {
			loop {
				img_notify.notified().await;

				let res = update_image(&client, &url_base, &url_list);
				let img_data = match res.await {
					Ok(data) => data,
					Err(err) => {
						match err_tx.send(err).await {
							Ok(_) => continue,
							Err(_) => break,
						}
					},
				};
				let time_str = img_data.timestamp.format("%H:%M").to_string();
				let date_str = (Local::today() != img_data.timestamp.date())
					.then(|| img_data.timestamp.format("%Y-%m-%d").to_string())
					.unwrap_or_default();

				log::info!("loaded image: {}", &img_data.timestamp);
				if let Some(err) = img_data.error.as_ref() {
					let err = anyhow::format_err!("{}", err)
						.context("camera error");
					err_tx.send(err).await.expect("failed to send error");
				}

				let image = {
					let mut img = img_data.image;
					if let Some(g) = opt.crop.as_ref() {
						img = img.crop_imm(g.x, g.y, g.width, g.height);
					}
					img.into_rgb8()
				};

				ui.upgrade_in_event_loop(move |ui| {
					let pixels = slint::SharedPixelBuffer::clone_from_slice(
						image.as_raw(), image.width(), image.height()
					);
					ui.set_bg_image(slint::Image::from_rgb8(pixels));
					ui.set_bg_time(time_str.into());
					ui.set_bg_date(date_str.into());
				});
			}
		}
	});

	let _status_task = spawn({
		let ui = ui.clone();
		async move {
			loop {
				let err = match err_rx.recv().await {
					Some(v) => v,
					None => break,
				};
				log::error!("{:#}", err);
				ui.upgrade_in_event_loop(move |ui| {

					//ui.get_status_log().as_ref().unwrap().as_any().downcast_ref::<VecModel<SharedString>>();
					ui.set_status_error(true);
					ui.set_status(error_showable(err));
				});
			}
		}
	});

	let _time_task = spawn({
		let ui = ui.clone();
		async move {
			let mut old_now = chrono::Local.timestamp(0, 0);
				ui.upgrade_in_event_loop(move |ui| {
					if !opt.show_date {
						ui.set_date("".into());
					}
					if !opt.show_time {
						ui.set_time("".into());
					}
				});
			loop {
				let now = chrono::Local::now();
				let new_time = if opt.show_time && now.time().minute() != old_now.time().minute() {
					let time = now.time();
					let time_str = time.format("%H:%M").to_string();
					Some(time_str)
				} else {
					None
				};
				let new_date = if opt.show_date && now.date() != old_now.date() {
					let date = now.date();
					// Remind about new year
					let date_fmt = if date.month() == 1 { "%A, %e. %B %Y" } else { "%A, %e. %B" };
					let date_str = date.format_localized(date_fmt, locale()).to_string();
					Some(date_str)
				} else {
					None
				};

				ui.upgrade_in_event_loop(move |ui| {
					if let Some(date) = new_date {
						ui.set_date(date.into());
					}
					if let Some(time) = new_time {
						ui.set_time(time.into());
					}
					ui.set_seconds(now.second() as i32);
				});

				old_now = now;
				time::sleep(time::Duration::from_millis(250)).await;
			};
	}});

	sync_task.await?;
	Ok(())
}

#[derive(Debug, Clone, Default)]
struct Geometry {
	width: u32,
	height: u32,
	x: u32,
	y: u32,
}

impl std::str::FromStr for Geometry {
	type Err = anyhow::Error;

	// parse WxH+X+Y
	fn from_str(s: &str) -> Result<Self, Self::Err> {
		fn parse_u32_pair(s: &str, delim: char) -> Option<(u32, u32)> {
			let v = s.split_once(delim)
				.and_then(|(_1, _2)|
					[_1, _2].into_iter()
						.map(u32::from_str)
						.collect::<Result<Vec<u32>, _>>()
						.ok()
				)?;

			Some((v.get(0).cloned()?, v.get(1).cloned()?))
		}
		s.split_once('+')
			.and_then(|(size, offset)|
				parse_u32_pair(size, 'x')
					.zip(parse_u32_pair(offset, '+')))
			.map(|((width, height), (x, y))| Self { width, height, x, y })
				.context("failed to parse crop format")
	}
}



#[derive(Debug, Copy, Clone)]
enum CamState {
	Idle,
	Capturing,
	Uploading,
	Processing,
	Ready,
}

impl Into<&str> for CamState {
	fn into(self) -> &'static str {
		match self {
			CamState::Idle => "Idle",
			CamState::Capturing => "Capturing",
			CamState::Uploading => "Uploading",
			CamState::Processing => "Processing",
			CamState::Ready => "Ready",
		}
	}
}

impl From<&str> for CamState {
	fn from(s: &str) -> Self {
		match s {
			"capture" => CamState::Capturing,
			"upload" => CamState::Uploading,
			"processing" => CamState::Processing,
			"ready" => CamState::Ready,
			_ => CamState::Idle,
		}
	}
}

async fn update_status(
		client: &reqwest::Client,
		url_base: &reqwest::Url,
		timestamp: &mut String,
	) -> Result<Option<CamState>> {

	let url_status = {
		let mut url = url_base.clone();
		url.query_pairs_mut().append_pair("newer", timestamp.as_str());
		url
	};

	#[derive(Deserialize)]
	struct CamLogLine(String,String,String,String,String,String);

	log::debug!("loading status from {} ...", &url_status);
	let log = client.get(url_status.clone())
		.header("Connection", "keep-alive")
		.send()
		.and_then(|resp| async { resp.error_for_status() })
		.and_then(http::Response::json::<Vec<CamLogLine>>)
		.await
		.context("failed to fetch status")?;

	let mut state = CamState::Idle;
	for line in &log {
		log::info!("camlog: {} - {} - {}", line.0, line.1, line.5);

		let info = &line.5;
		state = info.split_once(' ')
			.map(|(state, _)| state)
			.unwrap_or(&info)
			.split_once('=')
			.and_then(|(key, value)| (key == "state").then(|| CamState::from(value)))
			.unwrap_or(state);
	}
	if let Some(line) = log.last() {
		*timestamp = line.2.clone();
	}

	let state = if matches!(state, CamState::Idle) { None } else { Some(state) };
	Ok(state)
}

struct ImageData {
	image: DynamicImage,
	error: Option<String>,
	timestamp: DateTime<Local>,
}

async fn update_image(
		client: &reqwest::Client,
		url_base: &reqwest::Url, url_list: &reqwest::Url,
	) -> Result<ImageData> {

	log::debug!("loading image metadata from {} ...", &url_list);
	#[derive(Deserialize)]
	struct List {
		hugeimg: String,
		#[serde(rename="errorMsg")]
		error: Option<String>,
	}
	let list = client.get(url_list.clone())
		.send()
		.and_then(http::Response::json::<List>)
		.await
		.context("failed to fetch image list")?;

	const IMG_PATH_TS_FMT: &str = "%Y/%m/%d/%H%M_hu.jpg";
	let img_ts = Local.datetime_from_str(&list.hugeimg, IMG_PATH_TS_FMT)
		.context("failed to parse date time")?;

	let url_img = format!("{}/{}", &url_base, list.hugeimg);

	log::debug!("loading image from {} ...", &url_img);
	let image = client.get(url_img.clone())
		.send()
		.and_then(|resp| async { resp.error_for_status() })
		.and_then(|resp| resp.bytes())
		.await
		.context("failed to fetch image")
		.and_then(|jpeg_data|
			image::load_from_memory_with_format(&jpeg_data, image::ImageFormat::Jpeg)
				.context("failed to parse JPEG image")
		)?;

	Ok(ImageData {
		image,
		timestamp: img_ts,
		error: list.error,
	})
}

fn error_showable(err: anyhow::Error) -> slint::SharedString {
	format!("{}: {}", err, err.root_cause()).into()
}

fn locale() -> chrono::Locale {
	static LOCALE_INIT: Once = Once::new();
	static mut LOCALE: chrono::Locale = chrono::Locale::POSIX;

	unsafe {
		LOCALE_INIT.call_once(|| {
			match sys_locale::get_locale()
				.and_then(|locstr| chrono::Locale::try_from(locstr.as_str()).ok())
			{
				Some(loc) => LOCALE = loc,
				None => log::error!("failed to get system locale"),
			}
		});
		LOCALE
	}
}
