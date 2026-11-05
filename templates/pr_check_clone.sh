git clone --quiet --no-checkout --no-hardlinks -- {{ source }} {{ target }} && git -C {{ target }} checkout --quiet --detach {{ sha }}
