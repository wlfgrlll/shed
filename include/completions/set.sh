_set_comp() {
	local flags=( a b C e f h m n u v x )
	local options=(
		allexport
		emacs
		errexit
		hashall
		ignoreeof
		monitor
		noclobber
		noexec
		noglob
		nolog
		notify
		nounset
		pipefail
		verbose
		vi
		xtrace
	)
	case "$3" in
		-o|+o)
			compadd -a options
		;;
		*)
			case "$2" in
				+*) compadd -P '+' -a flags ;;
				-*) compadd -P '-' -a flags ;;
			esac
		;;
	esac
}
complete -F _set_comp set