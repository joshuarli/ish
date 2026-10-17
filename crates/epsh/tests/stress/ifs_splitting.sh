# IFS splitting on large strings — tests field_split performance
long="a:b:c:d:e:f:g:h:i:j:k:l:m:n:o:p:q:r:s:t:u:v:w:x:y:z"
IFS=:
i=0
while [ $i -lt 2000 ]; do
    set -- $long
    count=$#
    i=$((i + 1))
done
echo "ifs done: count=$count"
