# The Mac provisions these files after boot. Keep project overrides in the environment.
if [ -r /var/lib/atomics-localhost-tls/current/key.pem ]; then
    export ATOMICS_LOCALHOST_CERT="${ATOMICS_LOCALHOST_CERT-/var/lib/atomics-localhost-tls/current/cert.pem}"
    export ATOMICS_LOCALHOST_KEY="${ATOMICS_LOCALHOST_KEY-/var/lib/atomics-localhost-tls/current/key.pem}"
    export ASPNETCORE_Kestrel__Certificates__Default__Path="${ASPNETCORE_Kestrel__Certificates__Default__Path-/var/lib/atomics-localhost-tls/current/cert.pem}"
    export ASPNETCORE_Kestrel__Certificates__Default__KeyPath="${ASPNETCORE_Kestrel__Certificates__Default__KeyPath-/var/lib/atomics-localhost-tls/current/key.pem}"
fi
