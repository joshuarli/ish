# Many command substitutions — tests fork/pipe overhead
i=0
while [ $i -lt 500 ]; do
    x=$(echo $i)
    i=$((i + 1))
done
echo "comsubs done: $x"
