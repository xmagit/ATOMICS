{ openssh }:

# sshd's authorized_keys parser is internal to OpenSSH. Build the check with
# the guest's OpenSSH sources and configuration so both accept the same syntax.
openssh.overrideAttrs (_: {
  pname = "check-authorized-key";
  outputs = [ "out" ];
  preBuild = ''
    cp ${./check-authorized-key.c} check-authorized-key.c
    cat >> Makefile <<'EOF'
    key_objects = check-authorized-key.o auth-options.o $(P11OBJS) $(SKOBJS)
    check-authorized-key: $(key_objects) libssh.a $(LIBCOMPAT) ; \
      $(LD) -o $@ $(key_objects) $(LDFLAGS) -lssh -lopenbsd-compat -lssh -lopenbsd-compat $(TESTLIBS)
    EOF
  '';
  buildFlags = [ "check-authorized-key" ];
  installPhase = ''install -Dm755 check-authorized-key "$out/bin/check-authorized-key"'';
  doInstallCheck = false;
})
