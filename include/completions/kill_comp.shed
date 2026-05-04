_kill_comp() {
	case "$3" in
		-s|-l) compadd $(compgen -S -- $2) ;;
		*)
			case "$2" in
				-*) compadd -P '-' $(compgen -S -- "${2#-}") ;;
				*) compadd $(compgen -j -- "$2") ;;
			esac
		;;
	esac
}
complete -F _kill_comp kill