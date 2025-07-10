use std::os::windows::process::ExitStatusExt;
use std::process::ExitStatus;

use fxprof_processed_profile::{Profile, ReferenceTimestamp, SamplingInterval};

use super::etw_gecko;
use super::profile_context::ProfileContext;
use crate::shared::ctrl_c::CtrlC;
use crate::shared::included_processes::IncludedProcesses;
use crate::shared::prop_types::{ProfileCreationProps, RecordingMode, RecordingProps};
use crate::windows::elevated_helper::ElevatedHelperSession;

// Hello intrepid explorer! You may be in this code because you'd like to extend something,
// or are trying to figure out how various ETW things work. It's not the easiest API!
//
// Here are some useful things I've discovered along the way:
// - The useful ETW events are part of the Kernel provider. This Kernel provider uses "classic" "MOF" events,
//   not new-style XML manifest events. This makes them a bit more of a pain to work with, and they're
//   poorly documented. Luckily, ferrisetw does a good job of pulling out the schema even for MOF events.
// - When trying to decipher ETW providers, opcodes, keywords, etc. there are two tools that
//   are useful:
//   - `logman query providers Microsoft-Windows-Kernel-Process` (or any other provider)
//     will give you info about the provider, including its keywords and values. Handy for a quick view.
//   - `Get-WinEvent -ListProvider "Microsoft-Windows-Kernel-Process` (PowerShell) will
//     give you more info. The returned object contains everything you may want to know, but you'll
//     need to print it. For example:
//       - `(Get-WinEvent -ListProvider "Microsoft-Windows-Kernel-Process").Opcodes`
//          will give you the opcodes.
//       - `(Get-WinEvent -ListProvider "Microsoft-Windows-Kernel-Process").Events[5]` will give you details about
//          that event.
//   - To get information about them, you can use `wbemtest`. Connect to the default store, and run a query like
//     `SELECT * FROM meta_class WHERE __THIS ISA "Win32_SystemTrace"`, Double click that to get object information,
//     including decompiling to MOF (the actual struct). There's a class hierarchy here.
//   - But not all events show up in `wbemtest`! A MOF file is in the Windows WDK, "Include/10.0.22621.0/km/wmicore.mof".
//     But it seems to only contain "32-bit" versions of events (e.g. addresses are u32, not u64). I can't find
//     a fully up to date .mof.
//   - There are some more complex StackWalk events (see etw.rs for info) but I haven't seen them.

pub fn run(
    recording_mode: RecordingMode,
    recording_props: RecordingProps,
    profile_creation_props: ProfileCreationProps,
) -> Result<(Profile, ExitStatus), i32> {
    let timebase = std::time::SystemTime::now();
    let timebase = ReferenceTimestamp::from_system_time(timebase);

    let profile = Profile::new(
        profile_creation_props.profile_name(),
        timebase,
        SamplingInterval::from_nanos(1000000), // will be replaced with correct interval from file later
    );

    // Start xperf.
    let mut elevated_helper = ElevatedHelperSession::new(recording_props.output_file.clone())
        .unwrap_or_else(|e| panic!("Couldn't start elevated helper process: {e:?}"));
    elevated_helper
        .start_xperf(&recording_props, &profile_creation_props, &recording_mode)
        .unwrap();

    let included_processes = match recording_mode {
        RecordingMode::All => {
            let ctrl_c_receiver = CtrlC::observe_oneshot();
            eprintln!("Profiling all processes...");
            eprintln!("Press Ctrl+C to stop.");
            // TODO: Respect recording_props.time_limit, if specified
            // Wait for Ctrl+C.
            let _ = ctrl_c_receiver.blocking_recv();
            None
        }
        RecordingMode::Pid(pid) => {
            let ctrl_c_receiver = CtrlC::observe_oneshot();
            // TODO: check that process with this pid exists
            eprintln!("Profiling process with pid {pid}...");
            eprintln!("Press Ctrl+C to stop.");
            // TODO: Respect recording_props.time_limit, if specified
            // Wait for Ctrl+C.
            let _ = ctrl_c_receiver.blocking_recv();
            Some(IncludedProcesses {
                name_substrings: Vec::new(),
                pids: vec![pid],
            })
        }
        RecordingMode::Launch(process_launch_props) => {
            // Ignore Ctrl+C while the subcommand is running. The signal still reaches the process
            // under observation while we continue to record it.
            let mut ctrl_c_receiver = CtrlC::observe_oneshot();

            let mut pids = Vec::new();
            for _ in 0..process_launch_props.iteration_count {
                let mut child = std::process::Command::new(&process_launch_props.command_name);
                child.args(&process_launch_props.args);
                child.envs(process_launch_props.env_vars.iter().map(|(k, v)| (k, v)));
                let mut child = child.spawn().unwrap();

                pids.push(child.id());

                // Wait for the child to exit.
                //
                // TODO: Do the child waiting and the xperf control on different threads,
                // so that we can stop xperf immediately if we receive Ctrl+C. If they're on
                // the same thread, we might be blocking in wait() for a long time, and the
                // longer we take to handle Ctrl+C, the higher the chance that the user might
                // press Ctrl+C again, which would immediately terminate this process and not
                // give us a chance to stop xperf.
                let exit_status = child.wait().unwrap();
                if !process_launch_props.ignore_exit_code && !exit_status.success() {
                    eprintln!(
                        "Skipping remaining iterations due to non-success exit status: \"{exit_status}\""
                    );
                    break;
                }
            }

            // The launched subprocess is done. From now on, we want to terminate if the user presses Ctrl+C.
            ctrl_c_receiver.close();

            Some(IncludedProcesses {
                name_substrings: Vec::new(),
                pids,
            })
        }
    };

    eprintln!("Stopping xperf...");

    let (kernel_output_file, user_output_file) = elevated_helper
        .stop_xperf()
        .expect("Should have produced a merged ETL file");

    elevated_helper.shutdown();

    eprintln!("Processing ETL trace...");

    let arch = profile_creation_props
        .override_arch
        .clone()
        .unwrap_or(get_native_arch().to_string());

    let mut context = ProfileContext::new(
        profile,
        &arch,
        included_processes,
        profile_creation_props,
        None,
    );
    let extra_etls = match &user_output_file {
        Some(user_etl) => vec![user_etl.clone()],
        None => Vec::new(),
    };
    etw_gecko::process_etl_files(&mut context, &kernel_output_file, &extra_etls);

    if let Some(win_version) = winver::WindowsVersion::detect() {
        context.set_os_name(&format!("Windows {win_version}"))
    }

    let profile = context.finish();

    if !recording_props.keep_etl {
        std::fs::remove_file(&kernel_output_file).unwrap_or_else(|_| {
            panic!(
                "Failed to delete ETL file {:?}",
                kernel_output_file.to_str().unwrap()
            )
        });
        if let Some(user_output_file) = &user_output_file {
            std::fs::remove_file(user_output_file).unwrap_or_else(|_| {
                panic!(
                    "Failed to delete ETL file {:?}",
                    user_output_file.to_str().unwrap()
                )
            });
        }
    } else {
        eprintln!("ETL path: {}", kernel_output_file.to_str().unwrap());
        if let Some(user_output_file) = &user_output_file {
            eprintln!("User ETL path: {}", user_output_file.to_str().unwrap());
        }
    }

    Ok((profile, ExitStatus::from_raw(0)))
}

#[cfg(target_arch = "x86")]
fn get_native_arch() -> &'static str {
    "x86"
}

#[cfg(target_arch = "x86_64")]
fn get_native_arch() -> &'static str {
    "x86_64"
}

#[cfg(target_arch = "aarch64")]
fn get_native_arch() -> &'static str {
    "arm64"
}
