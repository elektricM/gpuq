# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_gpuq_global_optspecs
	string join \n api= h/help V/version
end

function __fish_gpuq_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_gpuq_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_gpuq_using_subcommand
	set -l cmd (__fish_gpuq_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c gpuq -n "__fish_gpuq_needs_command" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c gpuq -n "__fish_gpuq_needs_command" -s V -l version -d 'Print version'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "daemon" -d 'Run the daemon'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "check-config" -d 'Load and validate the config, then exit'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "status" -d 'Show the arbiter state, leases and queue'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "run" -d 'Run a command under a GPU lease'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "hold" -d 'Hold a lease until Ctrl-C'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "desktop" -d 'Show or change what gpuq does with the desktop ([desktop] in the config)'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "scan-drm" -d 'Print the processes that hold a card as JSON (run by the daemon through `elevate`)'
complete -c gpuq -n "__fish_gpuq_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c gpuq -n "__fish_gpuq_using_subcommand daemon" -s c -l config -d 'Config file (default: search the usual places)' -r -F
complete -c gpuq -n "__fish_gpuq_using_subcommand daemon" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand daemon" -s v -l verbose -d 'Log at debug level (GPUQ_LOG overrides it)'
complete -c gpuq -n "__fish_gpuq_using_subcommand daemon" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand check-config" -s c -l config -d 'Config file (default: search the usual places)' -r -F
complete -c gpuq -n "__fish_gpuq_using_subcommand check-config" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand check-config" -l strict -d 'Also fail when the VRAM probe does not work'
complete -c gpuq -n "__fish_gpuq_using_subcommand check-config" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand status" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand status" -l json -d 'Print the state as JSON'
complete -c gpuq -n "__fish_gpuq_using_subcommand status" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l class -d 'Priority class from gpuq\'s [classes] table' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l vram -d 'VRAM to reserve, e.g. 8G, 3000M or all' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l name -d 'Lease name shown in `gpuq status` (default: the command\'s basename)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l yield-signal -d 'Signal sent on a yield request, e.g. SIGUSR1 or TERM. Default: gpuq\'s `[run] yield_signal`, else SIGUSR1' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l card -d 'Card to run on: a name from `gpuq status`, a comma list such as gpu0,gpu1 for several cards at once, `all` for every card, or `any`. Default: any card with room' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l cards -d 'Run on this many cards at once, any that have room. --vram applies to each card' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l yieldable -d 'Signal the job when something more important needs the card'
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -l fallback -d 'If gpuq is not running, run the command anyway'
complete -c gpuq -n "__fish_gpuq_using_subcommand run" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l class -d 'Priority class from gpuq\'s [classes] table' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l vram -d 'VRAM to reserve, e.g. 8G, 3000M or all' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l name -d 'Lease name shown in `gpuq status`' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l card -d 'Card to hold: a name from `gpuq status`, a comma list, `all` or `any`. Default: any card with room' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l cards -d 'Hold this many cards at once, any that have room' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand hold" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -f -a "status" -d 'Show the state, why, and the processes on the card (the default)'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -f -a "release" -d 'Free the card now: move or stop the desktop as the policy says'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -f -a "restore" -d 'Give the desktop back now, and hold off automatic releases'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and not __fish_seen_subcommand_from status release restore help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from status" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from status" -l json -d 'Print the state as JSON'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from release" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from release" -l force -d 'Also when someone is logged in at the seat'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from release" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from restore" -l hold -d 'Hold off automatic releases this many seconds (default: hold_s from the config)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from restore" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from restore" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from help" -f -a "status" -d 'Show the state, why, and the processes on the card (the default)'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from help" -f -a "release" -d 'Free the card now: move or stop the desktop as the policy says'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from help" -f -a "restore" -d 'Give the desktop back now, and hold off automatic releases'
complete -c gpuq -n "__fish_gpuq_using_subcommand desktop; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c gpuq -n "__fish_gpuq_using_subcommand scan-drm" -l sysfs -r -F
complete -c gpuq -n "__fish_gpuq_using_subcommand scan-drm" -l pci -r
complete -c gpuq -n "__fish_gpuq_using_subcommand scan-drm" -l api -d 'Address of the gpuq control API (default: $GPUQ_API, else 127.0.0.1:9390)' -r
complete -c gpuq -n "__fish_gpuq_using_subcommand scan-drm" -s h -l help -d 'Print help'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "daemon" -d 'Run the daemon'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "check-config" -d 'Load and validate the config, then exit'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "status" -d 'Show the arbiter state, leases and queue'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "run" -d 'Run a command under a GPU lease'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "hold" -d 'Hold a lease until Ctrl-C'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "desktop" -d 'Show or change what gpuq does with the desktop ([desktop] in the config)'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "scan-drm" -d 'Print the processes that hold a card as JSON (run by the daemon through `elevate`)'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and not __fish_seen_subcommand_from daemon check-config status run hold desktop scan-drm help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and __fish_seen_subcommand_from desktop" -f -a "status" -d 'Show the state, why, and the processes on the card (the default)'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and __fish_seen_subcommand_from desktop" -f -a "release" -d 'Free the card now: move or stop the desktop as the policy says'
complete -c gpuq -n "__fish_gpuq_using_subcommand help; and __fish_seen_subcommand_from desktop" -f -a "restore" -d 'Give the desktop back now, and hold off automatic releases'
