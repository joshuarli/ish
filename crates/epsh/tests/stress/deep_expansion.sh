# Deep parameter expansion — tests recursive expansion
a=hello
b=${a:-world}
c=${b:-${a:-${b:-fallback}}}
d=${c:+${b:+${a:+nested}}}
i=0
while [ $i -lt 5000 ]; do
    x=${a:-${b:-${c:-${d:-deep}}}}
    y=${x%l*}
    z=${x#*l}
    i=$((i + 1))
done
echo "expansion done: $x $y $z"
