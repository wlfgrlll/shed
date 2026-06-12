_help_comp() {
	local tags=( $(help -l) )
	compadd -a tags
}
complete -f -F _help_comp help
