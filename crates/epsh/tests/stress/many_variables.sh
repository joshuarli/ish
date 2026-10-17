# Many variable assignments — tests var storage performance
i=0
while [ $i -lt 5000 ]; do
    eval "var_$i=$i"
    i=$((i + 1))
done
echo "vars done: $var_0 $var_4999"
