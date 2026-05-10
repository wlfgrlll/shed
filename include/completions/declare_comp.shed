_declare_comp() {
	case "$3" in
		-f|-F)
			local funcs=( $(declare -F | awk '{print $3}') )
			compadd -a funcs
		;;
		*) compadd -- -f -F -r -x -a -A -i -p ;;
	esac
}
complete -F _declare_comp declare