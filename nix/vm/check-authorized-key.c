/* Check one cidata row using the same key and option parsers as sshd. */
#include "includes.h"
#include <stdio.h>
#include "sshkey.h"
#include "authfile.h"
#include "auth-options.h"
#include "misc.h"

int main(void)
{
    char *line = NULL, *key;
    size_t size = 0;
    const char *reason;
    struct sshkey *parsed = sshkey_new(KEY_UNSPEC);

    if (parsed == NULL || getline(&line, &size, stdin) < 0)
        return 1;
    key = chop(line);
    skip_space(&key);
    if (sshkey_read(parsed, &key) == 0)
        return 0;

    const char *options = key;
    if (sshkey_advance_past_options(&key) != 0)
        return 1;
    skip_space(&key);
    if (sshkey_read(parsed, &key) != 0)
        return 1;
    if (sshauthopt_parse(options, &reason) == NULL) {
        fprintf(stderr, "Invalid SSH key options: %s\n", reason);
        return 1;
    }
    return 0;
}
