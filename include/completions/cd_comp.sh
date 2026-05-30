_cd_comp() {
	defer "shopt core.nullglob=$(shopt core.nullglob)"
	shopt core.nullglob=true
	local word=${COMP_WORDS[$COMP_CWORD]}

	COMPREPLY=()
	for match in ${word}*; do
		if [ -d "$match" ]; then
			push COMPREPLY $match/
		fi
	done
	local cdpath=$CDPATH
	while dir="${cdpath%%:*}"; cdpath="${cdpath#*:}"; do
		ran=1
		for match in "$dir/$word"*; do
			if [ -d "$match" ]; then
				push COMPREPLY ${match#$dir/}/
			fi
		done
	done
	if [ -n "$cdpath" ]; then
		cdpath="${cdpath%/}"
		for match in "$cdpath/$word"*; do
			if [ -d "$match" ]; then
				push COMPREPLY "${match#$cdpath/}"/
			fi
		done
	fi
}
complete -F _cd_comp cd
