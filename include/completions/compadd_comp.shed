_compadd_comp() {
	case "$2" in
		-)
			local flags=(P S d a)
			local descs=(
				"candidate prefix"
				"candidate suffix"
				"description array"
				"candidate array"
			)
			compadd -P '-' -d descs -a flags
		;;
		*)
			case "$3" in
				-d|-a)
					local vars=( $(compgen -v) )
					for var in "${vars[@]}"; do
						case $(type -s "$var") in
							array)
								compadd "$var"
							;;
						esac
					done
				;;
			esac
		;;
	esac
}