# This is the code for the prompt I currently use
# It makes use of the '\@funcname' function expansion escape sequence
# and the '-p' flag for echo which expands prompt escape sequences
#
# The final product looks sorta like this:
# ┏━ user@hostname
# ┃ 󰒓 1 job(s) running
# ┃ 🌐 192.168.1.777
# ┣━━ ~/path/to/pwd/
# ┗━ $ echo foo bar
# you might need nerd fonts to see the job line icon
# also the jobs/ssh lines are only visible if there is relevant info for them to display

# '\@function' expands to the output of `function`
export PS1="\@prompt "

prompt() {
	local topline="$(prompt_topline)"
	local jobsline="$(prompt_jobs_line)"
	local sshline="$(prompt_ssh_line)"
	local pwdline="$(prompt_pwd_line)"
	local dollarline="$(prompt_dollar_line)"
	local prompt="$topline$jobsline$sshline$pwdline\n$dollarline"

	echo -en "$prompt"
}
prompt_dollar_line() {
	local dollar="$(echo -p "\$ ")"
	local dollar="$(echo -e "\e[1;32m$dollar\e[0m")"
	echo -n "\e[1;34m┗━ $dollar "

}
prompt_jobs_line() {
	local job_count="$(echo -p '\j')"
	if [ "$job_count" -gt 0 ]; then
	  echo -n "\e[1;34m┃ \e[1;33m󰒓 $job_count job(s) running\e[0m\n"
	fi

}
prompt_pwd_line() {
	# the -p flag exposes prompt escape sequences like '\W'
	echo -p "\e[1;34m┣━━ \e[1;36m\W\e[1;32m/"

}
prompt_ssh_line() {
	local ssh_server="$(echo $SSH_CONNECTION | cut -f3 -d' ')"
	[ -n "$ssh_server" ] && echo -n "\e[1;34m┃ \e[1;39m🌐 $ssh_server\e[0m\n"

}
prompt_topline() {
	local user_and_host="\e[0m\e[1m$USER\e[1;36m@\e[1;31m$HOST\e[0m"
	local mode_text="$(prompt_mode)"
	echo -n "\e[1;34m┏━ $user_and_host $mode_text\n"

}
