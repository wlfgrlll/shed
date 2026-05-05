_hist_comp() {
	local short_flags=( n r )
	local long_flags=(
		delete
		ex
		restore
		count
		not
		json
	)
	local opts=(
		after
		lines-gt
		lines-lt
		before
		ends-with
		contains
		starts-with
		matches
		duration-gt
		duration-lt
		with-status
		with-token
		in-dir
		limit
		import
	)

	case $2 in
		--*)
			compadd -P '--' -a opts
			compadd -P '--' -a long_flags
		;;
		-*)
			compadd -P '-' -a short_flags
		;;
	esac
}
complete -F _hist_comp hist