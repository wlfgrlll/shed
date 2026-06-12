split() {
	[ "$#" -eq 2 ] || raise "Usage: split <%1> <%2> " 'pattern' 'string'
	[ -z "$2" ] && return
	local pat="$1"
	local parts="$2"
	local part

	while parts="${parts%${pat}}"; do :; done # strips all trailing delimiters

	parts="${parts}${pat}" # attaches a delimiter to the end

	while part="${parts%%${pat}*}" && parts="${parts#*${pat}}"; do
		quote "$part";
	done
}
