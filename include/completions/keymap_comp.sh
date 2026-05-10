_keymap_comp() {
	local -A modes=(
		[n]=normal
		[i]=insert
		[v]=visual
		[x]=command
		[o]=op_pending
		[r]=replace
		[e]=emacs
	)
	case "$2" in
		-*)
			local typed="${2#-}"
			local chars=() descs=()
			if [[ -z "$typed" ]]; then
				chars=( ${!modes[@]} );
				descs=( ${modes[@]} );
			else
				for c in ${!modes[@]}; do
					case "$typed" in
						*"$c"*) : ;;
						*) chars+=("$c"); descs+=("${modes[$c]}") ;;
					esac
				done
			fi
			compadd -a chars -d descs -P "$2"
		;;
	esac
}
complete -F _keymap_comp keymap