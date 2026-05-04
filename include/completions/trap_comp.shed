_trap_comp() {
	case "$3" in
		trap) compadd $(compgen -W "-l -p" -- "$2") ;;
		*)    compadd $(compgen -S -- "$2") ;;
	esac
}
complete -F _trap_comp trap