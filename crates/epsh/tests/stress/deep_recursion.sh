# Deep function recursion — tests scope stack
countdown() {
    if [ $1 -le 0 ]; then
        echo "done"
        return
    fi
    countdown $(($1 - 1))
}
countdown 200
