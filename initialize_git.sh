#!/bin/bash

# GitHub Secrets Setup Script
# Run this on your LOCAL machine (not VPS) after installing GitHub CLI

set -e

echo "Setting up GitHub Secrets using gh CLI..."
echo ""

# SSH_PRIVATE_KEY
echo "Setting SSH_PRIVATE_KEY..."
gh secret set SSH_PRIVATE_KEY -b"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACA6SosHom+eMPhVfRguDj0nBr95IVFRCPgJgHShDokjGgAAAKCWR8y0lkfM
tAAAAAtzc2gtZWQyNTUxOQAAACA6SosHom+eMPhVfRguDj0nBr95IVFRCPgJgHShDokjGg
AAAEDmXxdHyCWBkCEKavCWDwcZnTX+tl+uQ8gcRxo9BQ/dTzpKiweib54w+FV9GC4OPScG
v3khUVEI+AmAdKEOiSMaAAAAGGdpdGh1Yi1hY3Rpb25zQGFwZXgtcnVzdAECAwQF
-----END OPENSSH PRIVATE KEY-----"

# SSH_HOST
echo "Setting SSH_HOST..."
gh secret set SSH_HOST -b"165.22.31.49"

# SSH_USER
echo "Setting SSH_USER..."
gh secret set SSH_USER -b"root"

# SSH_PORT
echo "Setting SSH_PORT..."
gh secret set SSH_PORT -b"22"

# SSH_KNOWN_HOSTS
echo "Setting SSH_KNOWN_HOSTS..."
gh secret set SSH_KNOWN_HOSTS -b"# 165.22.31.49:22 SSH-2.0-OpenSSH_9.0p1 Ubuntu-1ubuntu7
|1|RNLeB6v130TT3BAI52psekbtWtk=|RZ7TS76y0a1ooKm+SEHSExcBCJI= ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQDJRrMreI4RM87igfv9SMsHfhecUMf2iCNujVkakYg5FVHxaB/o9eloq1pNsRK/jYt1RsZyUxPd9ykRs3SICu76hF+ed/AptTeeAaOtMn46Ew2Rbkfjkubwst91m1BBCa1xy2TEkj1REyq0/QsaZZBepKm2m03O/2fcJqY5gH+0L6CrFLr1ptZW8xjthPLR/f/7JvR1Fp4TfRyoqm0mSo2sti0vE0XaCJdm53MRxJl4CNOcPYUfK9eV+Yg+b8VJCPWDPcgLkDSYRXeI1Ks8ubu8BGziLH8/yz/MskkNR7eL7TNv++EDCr5oW+96GLiBVqBIfz+0rIhubgL/w3txYwfi4E0t8Xm3CUi14P1k21pfqm+igLwqCONBNFmxeghDk6HU4FcauDYR7+mvVNeBpOsXm7n+rUTCQjAHYsmKMCtwodTS3SxVP10I18O2Pw080cNtDGvpiVJfSvdEIM42CbUN1RZwaeBjIouE2S9pHCsx5AxhViLJ6il2TcPzKLzpdWk=
|1|iNyramVekHaI5xCbCUA8OMoAFbU=|M4El8GAC6cCLVqkMro9auKn+qFQ= ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBBPMd5Q07nSg8zSl4BPnMWeszKR4168h+moRdEgl34yAnkAizKdygXXXayCOw9sySUo345j0Bl6gSPpOmiRc/v0=
|1|QgIW2va1tvEc8iGDJ/FoQ4X2c+c=|rijT34IgAQ/g0gjdNIN37ytIUCI= ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDdP4eKDJm45ZzZyGNBi/gU8sGTTH6zJ6Afm9OmwU/aj"

echo ""
echo "✅ All secrets set successfully!"
echo ""
echo "Verifying secrets..."
gh secret list

echo ""
echo "🎉 GitHub Secrets setup complete!"
echo ""
echo "You can now push to your repository and GitHub Actions will deploy automatically."