_autocmd_comp() {
	local event_descs=(
	"Before command execution"
	"After command execution"
	"Before directory change"
	"After directory change"
	"After job finishes"
	"Before prompt draws"
	"After prompt draws"
	"Before vi mode change"
	"After vi mode change"
	"When opening history search"
	"When closing history search"
	"When choosing a history entry"
	"When starting tab completion"
	"When closing completion menu"
	"When choosing a completion candidate"
	"When the prompt has been idle past prompt.idle_timeout"
	"After timed command returns"
	"When shell exits"
	)
	local events=(
		pre-cmd
		post-cmd
		pre-change-dir
		post-change-dir
		on-job-finish
		pre-prompt
		post-prompt
		pre-mode-change
		post-mode-change
		on-history-open
		on-history-close
		on-history-select
		on-completion-start
		on-completion-cancel
		on-completion-select
		on-idle-timeout
		on-time-report
		on-exit
	)
	case "$3" in
		autocmd|-c)
			compadd -d event_descs -a events
		;;
		*)
			case "$2" in
				-*)
					compadd '-c'
				;;
			esac
		;;
	esac
}
complete -F _autocmd_comp autocmd