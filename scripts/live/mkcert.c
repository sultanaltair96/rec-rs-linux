/* A self-signed certificate for the RTMPS test, without the openssl CLI (the
 * dev image has libcrypto through openssl-devel but not the openssl binary).
 *
 *   gcc -O2 -o mkcert mkcert.c -lcrypto && ./mkcert server.crt server.key
 *
 * CN=localhost with DNS:localhost and IP:127.0.0.1 subject alternative names,
 * valid for 30 days. Only ever used by scripts/live/ against a local MediaMTX.
 */
#include <stdio.h>

#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/rsa.h>
#include <openssl/x509.h>
#include <openssl/x509v3.h>

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: mkcert CERT KEY\n");
        return 2;
    }
    EVP_PKEY *pkey = NULL;
    EVP_PKEY_CTX *ctx = EVP_PKEY_CTX_new_id(EVP_PKEY_RSA, NULL);
    if (!ctx || EVP_PKEY_keygen_init(ctx) <= 0 ||
        EVP_PKEY_CTX_set_rsa_keygen_bits(ctx, 2048) <= 0 ||
        EVP_PKEY_keygen(ctx, &pkey) <= 0) {
        fprintf(stderr, "mkcert: key generation failed\n");
        return 1;
    }
    EVP_PKEY_CTX_free(ctx);

    X509 *x = X509_new();
    X509_set_version(x, 2);
    ASN1_INTEGER_set(X509_get_serialNumber(x), 1);
    X509_gmtime_adj(X509_get_notBefore(x), 0);
    X509_gmtime_adj(X509_get_notAfter(x), 60L * 60L * 24L * 30L);
    X509_set_pubkey(x, pkey);
    X509_NAME *name = X509_get_subject_name(x);
    X509_NAME_add_entry_by_txt(name, "CN", MBSTRING_ASC,
                               (const unsigned char *)"localhost", -1, -1, 0);
    X509_set_issuer_name(x, name);
    X509V3_CTX v3;
    X509V3_set_ctx_nodb(&v3);
    X509V3_set_ctx(&v3, x, x, NULL, NULL, 0);
    X509_EXTENSION *ext = X509V3_EXT_conf_nid(
        NULL, &v3, NID_subject_alt_name, "DNS:localhost,IP:127.0.0.1");
    if (ext) {
        X509_add_ext(x, ext, -1);
        X509_EXTENSION_free(ext);
    }
    if (!X509_sign(x, pkey, EVP_sha256())) {
        fprintf(stderr, "mkcert: signing failed\n");
        return 1;
    }

    FILE *f = fopen(argv[2], "w");
    if (!f) {
        perror(argv[2]);
        return 1;
    }
    PEM_write_PrivateKey(f, pkey, NULL, NULL, 0, NULL, NULL);
    fclose(f);
    f = fopen(argv[1], "w");
    if (!f) {
        perror(argv[1]);
        return 1;
    }
    PEM_write_X509(f, x);
    fclose(f);
    X509_free(x);
    EVP_PKEY_free(pkey);
    return 0;
}
