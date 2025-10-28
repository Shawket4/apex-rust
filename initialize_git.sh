#!/bin/bash

# GitHub Repository Setup Script
# This script helps you initialize and push your Rust project to GitHub

set -e

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}GitHub Repository Setup${NC}"
echo -e "${GREEN}========================================${NC}"

# Check if git is installed
if ! command -v git &> /dev/null; then
    echo -e "${RED}Git is not installed. Please install git first.${NC}"
    echo "Ubuntu/Debian: sudo apt-get install git"
    echo "macOS: brew install git"
    exit 1
fi

# Check if gh CLI is installed
if ! command -v gh &> /dev/null; then
    echo -e "${YELLOW}GitHub CLI (gh) is not installed.${NC}"
    echo "Would you like to install it? (recommended)"
    read -p "Install gh CLI? (y/n): " install_gh
    
    if [ "$install_gh" = "y" ]; then
        if [[ "$OSTYPE" == "linux-gnu"* ]]; then
            # Linux installation
            curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | sudo dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg
            sudo chmod go+r /usr/share/keyrings/githubcli-archive-keyring.gpg
            echo "deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" | sudo tee /etc/apt/sources.list.d/github-cli.list > /dev/null
            sudo apt update
            sudo apt install gh -y
        elif [[ "$OSTYPE" == "darwin"* ]]; then
            # macOS installation
            brew install gh
        fi
        echo -e "${GREEN}GitHub CLI installed!${NC}"
    else
        echo -e "${YELLOW}Continuing without gh CLI (manual setup required)${NC}"
    fi
fi

echo ""
echo -e "${BLUE}Project Information:${NC}"
read -p "Enter repository name [trip-stats-rust]: " repo_name
repo_name=${repo_name:-trip-stats-rust}

read -p "Enter repository description: " repo_description
read -p "Make repository private? (y/n) [n]: " is_private
is_private=${is_private:-n}

if [ "$is_private" = "y" ]; then
    visibility="private"
else
    visibility="public"
fi

# Initialize git if not already initialized
if [ ! -d .git ]; then
    echo -e "${YELLOW}Initializing git repository...${NC}"
    git init
    echo -e "${GREEN}Git repository initialized${NC}"
else
    echo -e "${BLUE}Git repository already initialized${NC}"
fi

# Create .gitignore if it doesn't exist
if [ ! -f .gitignore ]; then
    echo -e "${YELLOW}Creating .gitignore...${NC}"
    cat > .gitignore << 'EOF'
# Rust
/target/
**/*.rs.bk
*.pdb
Cargo.lock

# Environment variables
.env
.env.local
.env.*.local

# IDE
.vscode/
.idea/
*.swp
*.swo
*~

# OS
.DS_Store
Thumbs.db

# Logs
*.log
logs/

# Database
*.db
*.sqlite

# Backups
*.bak
*.backup

# Compiled files
*.o
*.so
*.dylib

# Test coverage
coverage/
*.profraw
*.profdata

# Documentation
/target/doc/
EOF
    echo -e "${GREEN}.gitignore created${NC}"
fi

# Add all files
echo -e "${YELLOW}Adding files to git...${NC}"
git add .

# Initial commit
echo -e "${YELLOW}Creating initial commit...${NC}"
git commit -m "Initial commit: Trip Statistics Rust API" || echo "Nothing to commit or already committed"

# Create repository on GitHub
if command -v gh &> /dev/null; then
    echo -e "${YELLOW}Creating GitHub repository...${NC}"
    
    # Login to GitHub if not already logged in
    if ! gh auth status &> /dev/null; then
        echo -e "${BLUE}Please login to GitHub:${NC}"
        gh auth login
    fi
    
    # Create repository
    gh repo create "$repo_name" --${visibility} --source=. --remote=origin --description "$repo_description"
    
    echo -e "${GREEN}Repository created on GitHub!${NC}"
    
    # Push to GitHub
    echo -e "${YELLOW}Pushing to GitHub...${NC}"
    git branch -M main
    git push -u origin main
    
    echo -e "${GREEN}Code pushed to GitHub!${NC}"
else
    echo -e "${YELLOW}Manual GitHub setup required:${NC}"
    echo ""
    echo "1. Go to https://github.com/new"
    echo "2. Repository name: $repo_name"
    echo "3. Description: $repo_description"
    echo "4. Visibility: $visibility"
    echo "5. DO NOT initialize with README, .gitignore, or license"
    echo "6. Click 'Create repository'"
    echo ""
    read -p "Press Enter after creating the repository on GitHub..."
    
    echo ""
    read -p "Enter your GitHub username: " github_username
    
    # Add remote
    git remote add origin "https://github.com/$github_username/$repo_name.git" 2>/dev/null || git remote set-url origin "https://github.com/$github_username/$repo_name.git"
    
    # Push to GitHub
    echo -e "${YELLOW}Pushing to GitHub...${NC}"
    git branch -M main
    git push -u origin main
    
    echo -e "${GREEN}Code pushed to GitHub!${NC}"
fi

echo ""
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Repository Setup Complete!${NC}"
echo -e "${GREEN}========================================${NC}"
echo ""
echo -e "${BLUE}Next Steps:${NC}"
echo ""
echo "1. Setup GitHub Secrets for deployment:"
echo "   gh secret set SSH_PRIVATE_KEY < ~/.ssh/github_actions"
echo "   gh secret set SSH_KNOWN_HOSTS -b\"\$(ssh-keyscan YOUR_VPS_IP)\""
echo "   gh secret set SSH_HOST -b\"YOUR_VPS_IP\""
echo "   gh secret set SSH_USER -b\"YOUR_SSH_USER\""
echo "   gh secret set SSH_PORT -b\"22\""
echo ""
echo "   Or manually at: https://github.com/$github_username/$repo_name/settings/secrets/actions"
echo ""
echo "2. Run the VPS setup script on your server"
echo ""
echo "3. Push changes to trigger deployment:"
echo "   git add ."
echo "   git commit -m \"Your changes\""
echo "   git push"
echo ""
echo -e "${YELLOW}Useful Commands:${NC}"
echo "   gh repo view --web          # Open repository in browser"
echo "   gh workflow list            # List workflows"
echo "   gh run list                 # List workflow runs"
echo "   gh run watch                # Watch current workflow run"
echo "   gh secret list              # List secrets"
echo ""