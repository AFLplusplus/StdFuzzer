//! A libfuzzer-like fuzzer with llmp-multithreading support and restarts
//! The `launcher` will spawn new processes for each cpu core.
use mimalloc::MiMalloc;
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use core::time::Duration;
use std::{env, net::SocketAddr, path::PathBuf, ptr::addr_of_mut};

use clap::{self, Parser};
use libafl::{
    corpus::{Corpus, InMemoryCorpus, OnDiskCorpus},
    events::{launcher::Launcher, EventConfig},
    executors::{inprocess::InProcessExecutor, ExitKind},
    feedback_or,
    feedbacks::{CrashFeedback, MaxMapFeedback, TimeFeedback, TimeoutFeedback},
    fuzzer::{Fuzzer, StdFuzzer},
    generators::RandBytesGenerator,
    inputs::{BytesInput, HasTargetBytes},
    monitors::tui::{ui::TuiUI, TuiMonitor},
    mutators::{
        scheduled::{havoc_mutations, tokens_mutations, StdScheduledMutator},
        token_mutations::{I2SRandReplace, Tokens},
        StdMOptMutator,
    },
    observers::{CanTrack, HitcountsMapObserver, StdMapObserver, TimeObserver},
    schedulers::{
        powersched::PowerSchedule, IndexesLenTimeMinimizerScheduler, PowerQueueScheduler,
    },
    stages::{
        calibrate::CalibrationStage, power::StdPowerMutationalStage, StdMutationalStage,
        TracingStage,
    },
    state::{HasCorpus, StdState},
    Error, HasMetadata,
};
use libafl_bolts::{
    core_affinity::Cores,
    current_nanos,
    rands::StdRand,
    shmem::{ShMemProvider, StdShMemProvider},
    tuples::{tuple_list, Merge},
    AsSlice,
};
use libafl_targets::{
    edges_max_num, libfuzzer_initialize, libfuzzer_test_one_input, CmpLogObserver, CMPLOG_MAP,
    EDGES_MAP,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Parses a millseconds int into a [`Duration`], used for commandline arg parsing
fn timeout_from_millis_str(time: &str) -> Result<Duration, Error> {
    Ok(Duration::from_millis(time.parse()?))
}

#[derive(Debug, Parser)]
#[command(
    name = "StdFuzzer",
    about = "StdFuzzer is the reference implementation of a generic bit-level fuzzer with LibAFL",
    author = "Andrea Fioraldi <andreafioraldi@gmail.com>",
    version = VERSION
)]
struct Opt {
    #[arg(
        short,
        long,
        value_parser = Cores::from_cmdline,
        help = "Spawn a client in each of the provided cores. Broker runs in the 0th core. 'all' to select all available cores. 'none' to run a client without binding to any core. eg: '1,2-4,6' selects the cores 1,2,3,4,6.",
        name = "CORES"
    )]
    cores: Cores,

    #[arg(
        short = 'p',
        long,
        help = "Choose the broker TCP port, default is 1337",
        name = "PORT"
    )]
    broker_port: u16,

    #[arg(short = 'a', long, help = "Specify a remote broker", name = "REMOTE")]
    remote_broker_addr: Option<SocketAddr>,

    #[arg(short, long, help = "Set an initial corpus directory", name = "INPUT")]
    input: Vec<PathBuf>,

    #[arg(
        short,
        long,
        help = "Set the output directory, default is ./out",
        name = "OUTPUT",
        default_value = "./out"
    )]
    output: PathBuf,

    #[arg(
        value_parser = timeout_from_millis_str,
        short,
        long,
        help = "Set the execution timeout in milliseconds, default is 1000",
        name = "TIMEOUT",
        default_value = "1000"
    )]
    timeout: Duration,

    #[arg(
        short = 'x',
        long,
        help = "Feed the fuzzer with an user-specified list of tokens (often called \"dictionary\"",
        name = "TOKENS"
    )]
    tokens: Vec<PathBuf>,

    #[arg(
        long,
        help = "Disable unicode in the UI (for old terminals)",
        name = "DISABLE_UNICODE"
    )]
    disable_unicode: bool,
}

/// The main fn, `no_mangle` as it is a C symbol
#[no_mangle]
pub fn libafl_main() {
    let workdir = env::current_dir().unwrap();

    let opt = Opt::parse();

    let cores = opt.cores;
    let broker_port = opt.broker_port;
    let remote_broker_addr = opt.remote_broker_addr;
    let input_dirs = opt.input;
    let output_dir = opt.output;
    let token_files = opt.tokens;
    let timeout_ms = opt.timeout;
    // let cmplog_enabled = matches.is_present("cmplog");

    println!("Workdir: {:?}", workdir.to_string_lossy().to_string());

    let shmem_provider = StdShMemProvider::new().expect("Failed to init shared memory");

    let monitor = TuiMonitor::new(TuiUI::new(
        format!("LibAFL's StdFuzzer v{}", VERSION),
        !opt.disable_unicode,
    ));

    let mut run_client = |state: Option<_>, mut mgr, core_id| {
        // Create an observation channel using the coverage map
        let edges_observer = HitcountsMapObserver::new(unsafe {
            StdMapObserver::from_mut_ptr("edges", EDGES_MAP.as_mut_ptr(), edges_max_num())
        })
        .track_indices();

        // Create an observation channel to keep track of the execution time
        let time_observer = TimeObserver::new("time");

        // Create the Cmp observer
        let cmplog_observer =
            unsafe { CmpLogObserver::with_map_ptr("cmplog", addr_of_mut!(CMPLOG_MAP), true) };

        // Feedback to rate the interestingness of an input
        // This one is composed by two Feedbacks in OR
        let mut feedback = feedback_or!(
            // New maximization map feedback linked to the edges observer and the feedback state
            MaxMapFeedback::new(&edges_observer),
            // Time feedback, this one does not need a feedback state
            TimeFeedback::new(&time_observer)
        );

        // A feedback to choose if an input is a solution or not
        let mut objective = feedback_or!(CrashFeedback::new(), TimeoutFeedback::new());

        // If not restarting, create a State from scratch
        let mut state = state.unwrap_or_else(|| {
            StdState::new(
                // RNG
                StdRand::with_seed(current_nanos()),
                // Corpus that will be evolved, we keep it in memory for performance
                InMemoryCorpus::new(),
                // Corpus in which we store solutions (crashes in this example),
                // on disk so the user can get them after stopping the fuzzer
                OnDiskCorpus::new(output_dir.clone()).unwrap(),
                // States of the feedbacks.
                // The feedbacks can report the data that should persist in the State.
                &mut feedback,
                // Same for objective feedbacks
                &mut objective,
            )
            .unwrap()
        });

        // Create a dictionary if not existing
        state.metadata_or_insert_with(|| {
            Tokens::new()
                .add_from_files(&token_files)
                .expect("Could not read tokens files.")
        });

        // The actual target run starts here.
        // Call LLVMFUzzerInitialize() if present.
        let args: Vec<String> = env::args().collect();
        if libfuzzer_initialize(&args) == -1 {
            println!("Warning: LLVMFuzzerInitialize failed with -1")
        }

        let map_feedback = MaxMapFeedback::new(&edges_observer);
        let calibration = CalibrationStage::new(&map_feedback);

        // Setup a randomic Input2State stage
        let i2s =
            StdMutationalStage::new(StdScheduledMutator::new(tuple_list!(I2SRandReplace::new())));

        // Setup a MOPT mutator
        let mutator = StdMOptMutator::new(
            &mut state,
            havoc_mutations().merge(tokens_mutations()),
            7,
            5,
        )?;

        let power = StdPowerMutationalStage::new(mutator);

        // A minimization+queue policy to get testcasess from the corpus
        let scheduler = IndexesLenTimeMinimizerScheduler::new(
            &edges_observer,
            PowerQueueScheduler::new(&mut state, &edges_observer, PowerSchedule::FAST),
        );

        // A fuzzer with feedbacks and a corpus scheduler
        let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

        // The wrapped harness function, calling out to the LLVM-style harness
        let mut harness = |input: &BytesInput| {
            let target = input.target_bytes();
            let buf = target.as_slice();
            libfuzzer_test_one_input(buf);
            ExitKind::Ok
        };

        // Create the executor for an in-process function with one observer for edge coverage and one for the execution time
        let mut executor = InProcessExecutor::with_timeout(
            &mut harness,
            tuple_list!(edges_observer, time_observer),
            &mut fuzzer,
            &mut state,
            &mut mgr,
            timeout_ms,
        )?;

        // Secondary harness due to mut ownership
        let mut harness = |input: &BytesInput| {
            let target = input.target_bytes();
            let buf = target.as_slice();
            libfuzzer_test_one_input(buf);
            ExitKind::Ok
        };

        // Setup a tracing stage in which we log comparisons
        let tracing = TracingStage::new(InProcessExecutor::new(
            &mut harness,
            tuple_list!(cmplog_observer),
            &mut fuzzer,
            &mut state,
            &mut mgr,
        )?);

        // The order of the stages matter!
        let mut stages = tuple_list!(calibration, tracing, i2s, power);

        // In case the corpus is empty (on first run), reset
        if state.must_load_initial_inputs() {
            if input_dirs.is_empty() {
                // Generator of printable bytearrays of max size 32
                let mut generator = RandBytesGenerator::new(32);

                // Generate 8 initial inputs
                state
                    .generate_initial_inputs(
                        &mut fuzzer,
                        &mut executor,
                        &mut generator,
                        &mut mgr,
                        8,
                    )
                    .expect("Failed to generate the initial corpus");
                println!(
                    "We imported {} inputs from the generator.",
                    state.corpus().count()
                );
            } else {
                println!("Loading from {:?}", &input_dirs);
                // Load from disk
                state
                    .load_initial_inputs_multicore(
                        &mut fuzzer,
                        &mut executor,
                        &mut mgr,
                        &input_dirs,
                        &core_id,
                        &cores,
                    )
                    .unwrap_or_else(|_| {
                        panic!("Failed to load initial corpus at {:?}", &input_dirs)
                    });
                println!("We imported {} inputs from disk.", state.corpus().count());
            }
        }

        fuzzer.fuzz_loop(&mut stages, &mut executor, &mut state, &mut mgr)?;
        Ok(())
    };

    match Launcher::builder()
        .shmem_provider(shmem_provider)
        .configuration(EventConfig::from_build_id())
        .monitor(monitor)
        .run_client(&mut run_client)
        .cores(&cores)
        .broker_port(broker_port)
        .remote_broker_addr(remote_broker_addr)
        .stdout_file(Some("/dev/null"))
        .build()
        .launch()
    {
        Ok(_) | Err(Error::ShuttingDown) => (),
        Err(e) => panic!("{:?}", e),
    };
}
