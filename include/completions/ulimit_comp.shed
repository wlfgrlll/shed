_ulimit_comp() {
	local -A flags=(
		[n]="open file count"
		[s]="max stack size (bytes)"
		[u]="process count"
		[v]="virtual memory (bytes)"
		[c]="core dump file size (bytes)"
	)
	chars=( ${!flags[@]} );
	descs=( ${flags[@]} );

	case "$2" in
		-*) compadd -a chars -d descs -P "-" ;;
		*)
			case "$3" in ulimit) compadd -a chars -d descs -P '-' ;; esac
		;;
	esac
}
complete -F _ulimit_comp ulimit